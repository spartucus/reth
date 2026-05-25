# 第二章：存储架构

> 核心问题：合约的 `storage[key] = value` 最终存到哪里？State Trie 如何存储？

## 前置知识回顾

在第一章中，我们知道了：
- 执行区块后会产生 `state_root`（代表全局状态）
- `state_root` 是所有账户的 Merkle Patricia Trie 的根哈希
- 每个区块头包含四个 Merkle 根：`state_root`, `transactions_root`, `receipts_root`, `withdrawals_root`

本章解答：
- 这些 Trie 的节点存储在哪里？
- reth 如何组织磁盘上的数据？
- 为什么 8GB 内存可以运行 1.4TB 数据的全节点？

---

## 核心架构：三层存储

reth 使用**混合存储架构**，将数据分为三层：

```
┌───────────────────────────────────────────────────────────────────┐
│  reth 的三层存储架构                                               │
├───────────────────────────────────────────────────────────────────┤
│                                                                    │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  1. MDBX 数据库（热数据 - 可修改）                        │    │
│  │     文件位置: datadir/db/mdbx.dat                        │    │
│  │     大小: ~800 GB（以太坊主网）                           │    │
│  │                                                           │    │
│  │  存储内容：                                                │    │
│  │  ├─ 当前状态                                              │    │
│  │  │  • PlainAccountState      (address → account)        │    │
│  │  │  • PlainStorageState      (address + key → value)    │    │
│  │  │  • HashedAccounts         (hash(addr) → account)     │    │
│  │  │  • HashedStorages         (hash(addr+key) → value)   │    │
│  │  │                                                        │    │
│  │  ├─ Merkle Trie 节点                                      │    │
│  │  │  • AccountsTrie           (State Trie 中间节点)       │    │
│  │  │  • StoragesTrie           (Storage Tries 中间节点)    │    │
│  │  │                                                        │    │
│  │  └─ 最近的链数据                                          │    │
│  │     • Headers/Transactions/Receipts (最近几十万个区块)   │    │
│  │     • TransactionHashNumbers                             │    │
│  │     • BlockBodyIndices                                   │    │
│  │                                                           │    │
│  │  特点：                                                    │    │
│  │  ✅ B+ tree 结构，支持事务                                │    │
│  │  ✅ Memory-mapped I/O（mmap）零拷贝访问                   │    │
│  │  ✅ 高性能随机读写                                         │    │
│  │  ✅ 适合频繁修改的数据                                     │    │
│  └──────────────────────────────────────────────────────────┘    │
│                           ↓                                       │
│                    （数据变冷后迁移）                              │
│                           ↓                                       │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  2. Static Files（冷数据 - 不可变）                      │    │
│  │     文件位置: datadir/static_files/                      │    │
│  │     大小: ~600 GB（以太坊主网，压缩后）                   │    │
│  │                                                           │    │
│  │  存储内容（按 Segment 分类）：                             │    │
│  │  ├─ Headers                (区块头)                      │    │
│  │  ├─ Transactions           (交易数据)                     │    │
│  │  ├─ Receipts               (交易回执)                     │    │
│  │  ├─ TransactionSenders     (交易发送者)                   │    │
│  │  ├─ AccountChangeSets      (账户变更集)                   │    │
│  │  └─ StorageChangeSets      (存储变更集)                   │    │
│  │                                                           │    │
│  │  文件格式：NippyJar                                        │    │
│  │  • 列式存储 + 压缩（Zstd/Lz4）                            │    │
│  │  • 默认每 500,000 个区块一个文件                          │    │
│  │  • 文件名: static_file_{segment}_{start}_{end}           │    │
│  │                                                           │    │
│  │  特点：                                                    │    │
│  │  ✅ 不可变，写入后不再修改                                 │    │
│  │  ✅ 列式压缩，节省 40-60% 空间                            │    │
│  │  ✅ Memory-mapped I/O 快速读取                            │    │
│  │  ✅ 可按文件粒度备份/删除                                  │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                    │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  3. RocksDB（可选 - 特定大表）                            │    │
│  │     文件位置: datadir/rocksdb/                           │    │
│  │     大小: ~300 GB（如果启用）                             │    │
│  │                                                           │    │
│  │  存储内容（通过 --rocksdb.* 参数启用）：                   │    │
│  │  ├─ TransactionHashNumbers  (tx_hash → tx_number)       │    │
│  │  ├─ AccountsHistory         (账户历史索引)               │    │
│  │  └─ StoragesHistory         (存储历史索引)               │    │
│  │                                                           │    │
│  │  特点：                                                    │    │
│  │  ✅ LSM Tree，写入密集优化                                │    │
│  │  ✅ 适合范围查询                                          │    │
│  │  ✅ 可选启用（--rocksdb.all 或单独指定）                  │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                    │
│  总磁盘占用: ~1.4 TB（相比纯 MDBX 的 2.5 TB 节省 44%）          │
└───────────────────────────────────────────────────────────────────┘
```

---

## State Trie 的存储：从原始数据到 Merkle 根

### Account 结构详解

首先，理解 Account 的完整结构：

**类型来源：** `reth_primitives_traits::Account`（外部 crate `reth-primitives-traits`，在本仓库中通过 `crates/storage/db-api/src/tables/mod.rs` 引用）

```rust
pub struct Account {
    /// 账户 nonce（交易计数）
    pub nonce: u64,

    /// 账户余额
    pub balance: U256,

    /// 合约代码的哈希（仅合约账户有值）
    /// 如果是 EOA（外部账户），此字段为 None
    /// 如果是空合约，此字段为 KECCAK_EMPTY
    pub bytecode_hash: Option<B256>,
}
```

**关键点：**
- `bytecode_hash` 不是代码本身，而是代码的 keccak256 哈希
- 实际的合约代码存储在单独的 `Bytecodes` 表中
- 这样设计可以去重：相同代码的合约只存储一份

### 合约代码的存储

**文件位置：** `crates/storage/db-api/src/tables/mod.rs:381-384`

```rust
/// 存储合约字节码
table Bytecodes {
    type Key = B256;       // bytecode_hash (keccak256(code))
    type Value = Bytecode; // 实际的合约代码
}
```

**存储示例：**
```
部署两个相同代码的合约：

Contract A (0x1234...):
  PlainAccountState[0x1234] = Account {
      nonce: 1,
      balance: 0,
      bytecode_hash: Some(0xabcd...)
  }

Contract B (0x5678...):
  PlainAccountState[0x5678] = Account {
      nonce: 1,
      balance: 0,
      bytecode_hash: Some(0xabcd...)  // 同样的 hash
  }

Bytecodes 表:
  Bytecodes[0xabcd...] = Bytecode {
      bytecode: [0x60, 0x80, 0x60, 0x40, ...],
      state: Analyzed  // 已分析跳转表
  }

只存储一份代码！节省空间
```

**Bytecode 结构：**

`Bytecode` 是 `revm` 定义的枚举，有两个变体（源码：`revm-bytecode/src/bytecode.rs`）：

```rust
pub enum Bytecode {
    /// EIP-7702 委托字节码（Prague 硬分叉引入）
    /// 固定 23 字节：0xEF01（magic）+ 0x00（version）+ Address（20字节）
    /// EOA 账户可以将调用委托给另一个合约地址
    Eip7702(Arc<Eip7702Bytecode>),

    /// 经过 jump table 分析的传统字节码
    /// 存储时已预计算好所有合法 JUMP 目标，执行时无需重复分析
    LegacyAnalyzed(Arc<LegacyAnalyzedBytecode>),
}

// LegacyAnalyzed 内部结构
pub struct LegacyAnalyzedBytecode {
    bytecode: Bytes,       // 末尾补零的字节码（保证始终以 STOP 结尾）
    original_len: usize,   // 原始未填充的长度，用于还原真实字节码
    jump_table: JumpTable, // 合法 JUMPDEST 的位图，O(1) 跳转目标校验
}

// Eip7702 内部结构
pub struct Eip7702Bytecode {
    pub delegated_address: Address, // 委托的目标合约地址
    pub version: u8,                // 当前固定为 0x00
    pub raw: Bytes,                 // 原始字节：ef01 00 <address(20字节)>
}
```

### 合约代码载入流程

**执行合约调用时如何载入代码？**

**文件位置：** `crates/storage/storage-api/src/state.rs:53-71`

```rust
// StateProvider trait 提供的方法
fn account_code(&self, addr: &Address) -> ProviderResult<Option<Bytecode>> {
    // 1. 获取账户信息
    let acc = match self.basic_account(addr)? {
        Some(acc) => acc,
        None => return Ok(None),  // 账户不存在
    };

    // 2. 检查是否有代码
    if let Some(code_hash) = acc.bytecode_hash {
        if code_hash == KECCAK_EMPTY {
            return Ok(None)  // 空代码
        }
        // 3. 从 Bytecodes 表读取实际代码
        return self.bytecode_by_hash(&code_hash)
    }

    Ok(None)  // EOA 账户，无代码
}

// 实现示例（LatestStateProvider）
fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
    // 直接从 Bytecodes 表读取
    self.tx().get_by_encoded_key::<tables::Bytecodes>(code_hash)
        .map_err(Into::into)
}
```

**完整调用链：**
```
EVM 执行合约调用
  │
  ▼
1. 读取目标地址的 Account
   StateProvider::basic_account(0x1234...)
   → PlainAccountState[0x1234] = { bytecode_hash: Some(0xabcd...) }
  │
  ▼
2. 获取合约代码
   StateProvider::account_code(0x1234...)
   → StateProvider::bytecode_by_hash(0xabcd...)
   → Bytecodes[0xabcd...] = Bytecode { ... }
  │
  ▼
3. 载入 EVM 执行
   EVM::call(code, calldata, ...)
```

**性能优化：**
- MDBX 使用 mmap，数据库引擎层面确实是内存映射访问
- 对于简单固定类型（如 B256 键），MDBX 返回直接指向 mmap 内存的引用（零拷贝）
- 但 Bytecode 值读取时需要 Compact 反序列化，过程中有 copy_to_bytes()，并非零拷贝
- 相同代码只存储一份（去重）
- Analyzed 状态的代码已预分析跳转表（执行更快）

### Block 的存储方式：拆解而非整存

reth **不存储完整的 block 对象**，而是把 block 拆开分散到多张表。这是一个关键的设计决策。

**一个区块的组成及其存储位置：**

```
Block N
├── Header
│   └── Headers[N]               → MDBX（同步期间），之后迁移到 static files Headers segment
│
├── Body
│   ├── Transactions
│   │   ├── BlockBodyIndices[N]  → MDBX（永久）
│   │   │   = { first_tx_num: 1000, tx_count: 5 }   ← 指向交易范围
│   │   └── Transactions[1000..1004]  → MDBX（同步期间），之后迁移到 static files Transactions
│   │
│   ├── Ommers (叔块)
│   │   └── BlockOmmers[N]       → MDBX（永久，叔块数量少）
│   │
│   └── Withdrawals (提款)
│       └── BlockWithdrawals[N]  → MDBX（永久）
│
└── [关联数据，非 block 本身]
    ├── Receipts[1000..1004]     → MDBX（Execution 后写入），之后迁移到 static files Receipts
    ├── TransactionSenders[1000..1004]  → MDBX（SenderRecovery 阶段写入）
    └── HeaderNumbers[block_hash] → N   → MDBX（永久，反向查找）
```

**重建完整 block 需要联查多张表：**

```rust
// 查询 block 100 的完整数据
let header  = provider.header_by_number(100)?;
//            → Headers[100] 或 static_file_headers.row(100)

let indices = provider.block_body_indices(100)?;
//            → BlockBodyIndices[100] = { first_tx_num: 250, tx_count: 3 }

let txs     = provider.transactions_by_tx_range(250..253)?;
//            → Transactions[250], [251], [252]（MDBX 或 static files）

let ommers  = provider.ommers(100)?;
//            → BlockOmmers[100]
```

**为什么不把 block 存成一个整体？**

| 原因 | 说明 |
|------|------|
| 避免重复数据 | Receipts、TransactionSenders 等都按 TxNumber 索引，和 Transactions 天然对齐 |
| 细粒度访问 | 只查 header 不用读 body，只查 tx 不用读 header，减少不必要的 I/O |
| 压缩效率更高 | Static files 按列压缩：parent_hash 一列、timestamp 一列，相同类型数据聚合压缩效果更好 |
| 去重 | 多个 block 的 transactions 按全局 TxNumber 连续存储，没有 block 边界开销 |

---

### MDBX 完整表清单

MDBX 共有 30 张表，按用途分为六类：

**1. 当前状态（永久保留，不可剪枝）**

| 表名 | Key | Value | 用途 |
|------|-----|-------|------|
| `PlainAccountState` | Address | Account | 账户当前状态（nonce/balance/codehash） |
| `PlainStorageState` | Address | StorageEntry (DupSort) | 合约存储当前值 |
| `Bytecodes` | B256 (code hash) | Bytecode | 合约字节码（按 hash 去重） |
| `HashedAccounts` | keccak256(Address) | Account | 哈希化账户（Trie 构建用） |
| `HashedStorages` | keccak256(Address) | StorageEntry (DupSort) | 哈希化存储（Trie 构建用） |

**2. Merkle Patricia Trie 节点（永久保留）**

| 表名 | Key | Value | 用途 |
|------|-----|-------|------|
| `AccountsTrie` | StoredNibbles (路径) | BranchNodeCompact | 账户状态树中间节点（计算 state root） |
| `StoragesTrie` | B256 | StorageTrieEntry (DupSort) | 合约存储树中间节点 |

**3. 索引/反向查找表（永久保留）**

| 表名 | Key | Value | 用途 |
|------|-----|-------|------|
| `HeaderNumbers` | BlockHash | BlockNumber | 区块哈希 → 高度 |
| `TransactionHashNumbers` | TxHash | TxNumber | 交易哈希 → 序号 |
| `TransactionBlocks` | TxNumber (最大) | BlockNumber | 交易序号 → 所在块 |
| `BlockBodyIndices` | BlockNumber | `{first_tx_num, tx_count}` | 块包含的交易范围 |

**4. 历史变更数据（可剪枝，用于历史状态查询）**

| 表名 | Key | Value | 用途 |
|------|-----|-------|------|
| `AccountChangeSets` | BlockNumber (DupSort) | AccountBeforeTx | 每个块中账户的变更前状态 |
| `StorageChangeSets` | BlockNumber+Address (DupSort) | StorageEntry | 每个块中存储的变更前值 |
| `AccountsHistory` | Address+最大块号 (sharded) | BlockNumberList | "账户在哪些块变过" 索引 |
| `StoragesHistory` | Address+Slot+最大块号 | BlockNumberList | "存储在哪些块变过" 索引 |
| `TransactionSenders` | TxNumber | Address | 交易发送者缓存（避免重复恢复签名） |

**5. 区块原始数据（同步期间临时存放，之后迁移到 static files 并剪枝）**

| 表名 | 最终去向 |
|------|---------|
| `CanonicalHeaders` | static files Headers segment |
| `Headers` | static files Headers segment |
| `Transactions` | static files Transactions segment |
| `Receipts` | static files Receipts segment（由 StaticFileProducer 迁移） |
| `BlockOmmers` | 永久保留在 MDBX（叔块数量少） |
| `BlockWithdrawals` | 永久保留在 MDBX |

**6. 系统元数据（永久保留）**

| 表名 | 用途 |
|------|------|
| `StageCheckpoints` | 各同步阶段的进度（15 个 stage） |
| `StageCheckpointProgresses` | Stage 首次同步的详细进度 |
| `PruneCheckpoints` | 各剪枝段已剪到哪个块 |
| `ChainState` | 最后 finalized block、最后 safe block |
| `VersionHistory` | 客户端版本历史 |
| `Metadata` | 通用键值元数据 |
| `HeaderTerminalDifficulties` | PoW 时代的总难度（已废弃） |

---

### 数据冗余的四层结构

reth 在 MDBX 中维护了多层数据，看似冗余，实则各有用途：

```rust
// 第一层：原始账户状态（未 hash）
table PlainAccountState {
    type Key = Address;              // 0x1234abcd...
    type Value = Account;            // { nonce: 5, balance: 100 ETH, bytecode_hash: Some(...) }
}

// 第二层：Hashed 账户状态（为 MPT 准备）
table HashedAccounts {
    type Key = B256;                 // keccak256(0x1234abcd...)
    type Value = Account;            // 同样的 Account 数据
}

// 第三层：Merkle Trie 的中间节点
table AccountsTrie {
    type Key = StoredNibbles;        // Trie 路径（nibbles）
    type Value = BranchNodeCompact;  // 分支节点（16 个子节点的 hash）
}

// 合约代码（独立存储，去重）
table Bytecodes {
    type Key = B256;                 // bytecode_hash
    type Value = Bytecode;           // 合约代码
}

// 存储数据同理
table PlainStorageState { ... }      // 原始存储
table HashedStorages { ... }         // Hashed 存储
table StoragesTrie { ... }           // Storage Trie 节点
```

**为什么需要这么多层？**

| 表名 | 用途 | 为什么需要 |
|------|------|-----------|
| `PlainAccountState` | RPC 查询、执行交易 | 需要原始地址（不能用 hash） |
| `HashedAccounts` | 计算 state_root | MPT 要求 key 必须是 hash，且 hash 后有序 |
| `AccountsTrie` | 增量更新 state_root | 存储中间节点，避免每次从头构建整棵树 |
| `Bytecodes` | 存储合约代码 | 独立存储，去重（相同代码只存一份） |

### State Root 的计算流程

**文件位置：** `crates/trie/trie/src/trie.rs`

```rust
// StateRoot 需要两个数据源
pub struct StateRoot<T, H> {
    /// 读取旧的 Trie 节点（从 AccountsTrie 表）
    pub trie_cursor_factory: T,

    /// 读取最新的 Hashed 数据（从 HashedAccounts 表）
    pub hashed_cursor_factory: H,

    /// 被修改的账户前缀集合（优化：只重新计算变化的分支）
    pub prefix_sets: TriePrefixSets,
}
```

**完整计算流程：**

```
┌─────────────────────────────────────────────────────────────────┐
│  计算 State Root（区块 N 执行后）                                │
└─────────────────────────────────────────────────────────────────┘

步骤 1: 执行交易，产生状态变更
    ↓
BundleState {
    // 内存中的变更（100 个账户被修改）
    accounts: HashMap<Address, AccountInfo>,
    storage: HashMap<Address, HashMap<B256, U256>>,
}
    ↓
步骤 2: 写入数据库（多表同时更新）
    ↓
┌──────────────────────────────────────────────────────────────┐
│ MDBX 数据库                                                   │
│                                                               │
│ PlainAccountState ← 写入 100 个账户的新状态                   │
│    0x1234... → Account { nonce: 6, balance: 95 ETH }        │
│    0x5678... → Account { nonce: 3, balance: 55 ETH }        │
│    ...                                                        │
│         ↓ (同时)                                              │
│ HashedAccounts ← 写入 100 个账户的 hashed 版本                │
│    keccak(0x1234...) → Account { nonce: 6, ... }            │
│    keccak(0x5678...) → Account { nonce: 3, ... }            │
│    ...                                                        │
└──────────────────────────────────────────────────────────────┘
    ↓
步骤 3: 调用 StateRoot::root_with_updates()
    ↓
┌──────────────────────────────────────────────────────────────┐
│ StateRoot 算法                                                │
│                                                               │
│ 1. 打开两个 Cursor:                                          │
│    TrieCursor        → 读取 AccountsTrie（旧的 trie 节点）   │
│    HashedCursor      → 读取 HashedAccounts（最新数据）       │
│                                                               │
│ 2. TrieWalker 同时遍历两个数据源:                             │
│    for hashed_account in HashedAccounts {                    │
│        if account 被修改 {                                    │
│            // 重新计算从该叶子到根的路径上的所有节点           │
│            update_branch_nodes(path_to_root);                │
│        } else {                                               │
│            // 复用旧的 hash                                   │
│            reuse_old_hash(old_trie_node);                    │
│        }                                                      │
│    }                                                          │
│                                                               │
│ 3. 计算示例（1000 万个账户，修改了 100 个）:                  │
│                                                               │
│    旧 State Trie:                                            │
│         Root (hash: 0xabc...)                                │
│          ├─ Branch A                                         │
│          │   ├─ 账户 0x1234... ← 被修改了                     │
│          │   └─ 账户 0x1235...                               │
│          ├─ Branch B (未变化) ← 复用旧 hash                  │
│          └─ Branch C (未变化) ← 复用旧 hash                  │
│                                                               │
│    新 State Trie:                                            │
│         Root (hash: 0xdef...) ← 新计算                       │
│          ├─ Branch A (hash 变了) ← 重新计算                  │
│          │   ├─ 账户 0x1234... (新状态) ← 重新计算            │
│          │   └─ 账户 0x1235... (未变)                        │
│          ├─ Branch B (hash 不变) ← 复用                      │
│          └─ Branch C (hash 不变) ← 复用                      │
│                                                               │
│    只重新计算了 log(N) 个节点，而不是 N 个！                  │
│                                                               │
│ 4. 返回:                                                      │
│    (root_hash, TrieUpdates)                                  │
│     ↓             ↓                                           │
│     写入块头      写入 AccountsTrie 表（更新受影响的节点）     │
└──────────────────────────────────────────────────────────────┘
    ↓
步骤 4: 写入块头
    ↓
BlockHeader {
    state_root: 0xdef...,  ← 代表全部 1000 万个账户的状态
    transactions_root: ...,
    receipts_root: ...,
    ...
}
```

**关键优化：增量更新**

```
全量重建（不可行）:
  遍历 1000 万个账户 → 构建完整 MPT → 计算 root
  时间复杂度: O(N)，需要几十分钟

增量更新（reth 的做法）:
  只遍历被修改的 100 个账户 → 更新受影响的分支 → 计算 root
  时间复杂度: O(M * log N)，其中 M = 修改的账户数
  实际耗时: 几百毫秒
```

---

## Static Files：冷数据归档

### 什么是 Static Files？

**文件位置：** `crates/storage/provider/src/providers/static_file/`

Static Files 是 reth 的冷数据归档方案，将不再修改的历史链数据从 MDBX 迁移到压缩的不可变文件中。

### 六种数据段（Segments）

```rust
// crates/static-file/types/src/segment.rs
pub enum StaticFileSegment {
    /// 区块头（CanonicalHeaders, Headers, HeaderTerminalDifficulties）
    Headers,

    /// 交易数据（Transactions 表）
    Transactions,

    /// 交易回执（Receipts 表）
    Receipts,

    /// 交易发送者（TransactionSenders 表）
    TransactionSenders,

    /// 账户变更集（AccountChangeSets 表）
    /// 用于历史状态查询和回滚
    AccountChangeSets,

    /// 存储变更集（StorageChangeSets 表）
    /// 用于历史状态查询和回滚
    StorageChangeSets,
}
```

### 三种不同的写入路径

这 6 个 Segment 并非都通过相同的方式写入 Static Files：

```
Headers      ──→ 【直接写入】Headers 同步阶段（HeaderStage）直接写入 static files
                  不经过 MDBX，MDBX 中的 CanonicalHeaders/Headers 表在同步完成后清空

Transactions ──→ 【直接写入】Bodies 同步阶段（BodiesStage）直接写入 static files
                  不经过 MDBX，MDBX 中的 Transactions 表在同步完成后清空

Receipts     ──→ 【先写 MDBX，再迁移】
                  1. Execution 阶段执行交易，结果写入 MDBX 的 Receipts 表
                  2. StaticFileProducer::run() 读取 MDBX，写入 static files
                  3. Pruner 从 MDBX 删除已迁移的 Receipts

TransactionSenders  ──→ 目前不自动迁移（StaticFileSegment 中存在但未被 Producer 处理）
AccountChangeSets   ──→ 目前不自动迁移
StorageChangeSets   ──→ 目前不自动迁移
```

**StaticFileProducer 实际只处理 Receipts**（源码：`static_file_producer.rs`）：

```rust
// StaticFileTargets 结构体只有 receipts 字段
pub struct StaticFileTargets {
    pub receipts: Option<RangeInclusive<BlockNumber>>,
    // 注意：没有 headers、transactions 等字段！
}

// StaticFileProducerInner::run 只添加 Receipts segment
let mut segments = Vec::new();
if let Some(block_range) = targets.receipts.clone() {
    segments.push((Box::new(segments::Receipts), block_range));  // 只有这一个！
}
// Headers 和 Transactions 由同步阶段直接写入，不需要 Producer 迁移
```

### NippyJar 文件格式

**文件位置：** `crates/storage/nippy-jar/src/lib.rs`

```rust
/// NippyJar 是为不可变数据设计的专用存储格式
///
/// 特点：
/// - 列式存储：同一列的数据连续存储，压缩效果更好
/// - Memory-mapped I/O：零拷贝读取
/// - 压缩支持：Zstd, Lz4 等
pub struct NippyJar<H> {
    /// 列数（例如 Header 有 15 列：parent_hash, number, timestamp...）
    columns: usize,

    /// 行数（区块数量）
    rows: usize,

    /// 压缩算法
    compressor: Option<Compressors>,

    /// 最大未压缩行大小
    max_row_size: usize,

    /// 数据文件路径
    path: PathBuf,
}
```

**文件结构示例：**

```
static_file_headers_0_499999/
├── headers-0-499999.jar       ← 主数据文件（列式存储 + 压缩）
│   ├─ Column 0: parent_hash   (500,000 个 B256，压缩后)
│   ├─ Column 1: number        (500,000 个 u64，压缩后)
│   ├─ Column 2: timestamp     (500,000 个 u64，压缩后)
│   └─ ...                     (共 15 列)
│
├── headers-0-499999.off       ← Offset 文件（每行数据的偏移量）
│   [0, 234, 456, 789, ...]    (500,000 个 offset)
│
├── headers-0-499999.idx       ← 索引文件
└── headers-0-499999.conf      ← 配置文件（元数据）
```

### 数据迁移流程

**文件位置：** `crates/static-file/static-file/src/static_file_producer.rs`

```
触发条件：
  - 区块数达到阈值（默认 500,000 个区块）
  - 或手动触发（reth stage run static-file）

迁移流程：
┌─────────────────────────────────────────────────────────────────┐
│ StaticFileProducer::run(targets)                                 │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
对每个 Segment 并行执行：
┌─────────────────────────────────────────────────────────────────┐
│ Segment::copy_to_static_files(provider, block_range)            │
│                                                                  │
│ 以 Receipts 为例：                                               │
│                                                                  │
│ 1. 获取 Static File 写入器                                       │
│    let mut writer = provider.get_static_file_writer(            │
│        block_start,                                             │
│        StaticFileSegment::Receipts                              │
│    )?;                                                           │
│                                                                  │
│ 2. 打开 MDBX 游标                                                │
│    let mut cursor = provider                                    │
│        .tx_ref()                                                │
│        .cursor_read::<tables::Receipts>()?;                     │
│                                                                  │
│ 3. 逐块读取并写入                                                │
│    for block in 0..=499999 {                                    │
│        // 从 MDBX 读取该块的所有 receipts                        │
│        let receipts = cursor.walk_range(                        │
│            block_body_indices.tx_num_range()                    │
│        )?;                                                       │
│                                                                  │
│        // 写入 Static File（列式组织 + 压缩）                    │
│        writer.append_receipts(receipts)?;                       │
│    }                                                             │
│                                                                  │
│ 4. 完成写入                                                      │
│    writer.commit()?;                                            │
│                                                                  │
│ 注意：此时 MDBX 中的数据还在！                                   │
│       实际删除由 Pruner 负责（crates/prune/）                    │
└─────────────────────────────────────────────────────────────────┘
```

**数据流向图：**

```
MDBX 数据库                           Static Files
datadir/db/data.mdb                   datadir/static_files/

┌──────────────────┐                  ┌──────────────────────┐
│ Receipts 表      │                  │ receipts/            │
│                  │  读取             │                      │
│ TxNum → Receipt  │ ────────┐        │ static_file_         │
│   0 → Receipt0   │         │        │   receipts_0_499999/ │
│   1 → Receipt1   │         │        │   ├── .jar           │
│   ...            │         ▼        │   ├── .off           │
│   999999→Receipt │    ┌─────────┐   │   └── .conf          │
└──────────────────┘    │ 列式组织 │   │                      │
                        │   +      │   │ static_file_         │
                        │ 压缩     │───┤   receipts_500000_   │
                        └─────────┘   │   999999/            │
                                      └──────────────────────┘

（可选）删除旧数据 ◄─── Pruner
```

### 读取示例

```rust
// 用户查询历史交易
// eth_getTransactionByHash(0xabc...)

// 1. 查找交易号
let tx_number = provider
    .transaction_id_by_hash(hash)?  // 查 MDBX 或 RocksDB
    .ok_or(NotFound)?;              // tx_number = 12,345,678

// 2. 判断数据位置
// tx_number 12,345,678 对应区块约 24,691
// 这个区块在 static file 范围内（0-499,999）

// 3. StaticFileProvider 读取
let static_file = provider.static_file_provider();
let tx = static_file.transaction_by_number(tx_number)?;

// 内部流程：
// a) 确定文件：static_file_transactions_0_499999.jar
// b) 读取 offset 文件：.off[tx_number] = 字节偏移量
// c) mmap 读取 .jar 文件对应位置
// d) 解压（如果有压缩）
// e) 反序列化返回 Transaction
```

---

## Memory-Mapped I/O：核心技术

### 为什么 MDBX 和 Static Files 都用 mmap？

**关键事实：它们都是磁盘文件，只是访问方式特殊！**

```
┌─────────────────────────────────────────────────────────────────┐
│  传统文件读写 vs mmap                                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  传统方式（read/write 系统调用）:                                 │
│    1. 应用调用 read(fd, buffer, size)                           │
│    2. 内核从磁盘读取到 kernel buffer                              │
│    3. 内核拷贝数据到 user buffer                                 │
│    4. 应用处理数据                                               │
│                                                                  │
│    问题：                                                         │
│    ❌ 数据在内存中有两份（kernel + user）                         │
│    ❌ 每次读写都要系统调用（性能开销）                             │
│    ❌ 应用需要管理缓冲区                                          │
│                                                                  │
│  ───────────────────────────────────────────────────────────── │
│                                                                  │
│  mmap 方式:                                                      │
│    1. 应用调用 mmap(fd, size) 一次                               │
│    2. 建立虚拟地址到文件的映射                                    │
│    3. 应用直接访问 addr[offset]                                  │
│    4. Page fault 自动加载数据到 page cache                       │
│                                                                  │
│    优势：                                                         │
│    ✅ 零拷贝（数据只在 page cache 中）                            │
│    ✅ 减少系统调用（只有初始 mmap 和 page fault）                 │
│    ✅ 操作系统自动管理缓存（LRU）                                  │
│    ✅ 多进程可共享 page cache（节省内存）                         │
└─────────────────────────────────────────────────────────────────┘
```

### 虚拟地址空间 vs 物理内存

**核心概念：8GB 内存可以访问 200GB 文件！**

```
┌─────────────────────────────────────────────────────────────────┐
│  物理内存（RAM）: 8GB                                             │
├─────────────────────────────────────────────────────────────────┤
│  0x00000000  ┌──────────────┐                                    │
│              │   Kernel     │  2GB                               │
│  0x80000000  ├──────────────┤                                    │
│              │   Process A  │  1GB                               │
│              ├──────────────┤                                    │
│              │   Process B  │  2GB                               │
│              ├──────────────┤                                    │
│              │   Page Cache │  3GB  ← MDBX/Static Files 的数据   │
│  0xFFFFFFFF  └──────────────┘                                    │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│  虚拟地址空间（64位进程）: 128 TB                                 │
├─────────────────────────────────────────────────────────────────┤
│  0x0000000000000000  ┌──────────────────────┐                    │
│                      │   程序代码段          │  100 MB           │
│                      ├──────────────────────┤                    │
│                      │   堆（malloc）        │  500 MB           │
│                      ├──────────────────────┤                    │
│  0x00007F0000000000  │   mmap 区域           │                    │
│                      │   ┌──────────────┐   │                    │
│                      │   │ data.mdb     │   │  200 GB           │
│                      │   │ (MDBX)       │   │  ↑                │
│                      │   └──────────────┘   │  只是虚拟地址映射  │
│                      │   ┌──────────────┐   │  不占物理内存！    │
│                      │   │ *.jar        │   │  50 GB            │
│                      │   │ (Static File)│   │                    │
│                      │   └──────────────┘   │                    │
│                      ├──────────────────────┤                    │
│                      │   栈                 │  8 MB              │
│  0x00007FFFFFFFFFFF  └──────────────────────┘                    │
└─────────────────────────────────────────────────────────────────┘

关键点：
  ✅ 虚拟地址空间足够大（128 TB），可以映射 200GB 文件
  ✅ 只有实际访问的页面才占用物理内存
  ✅ 物理内存不足时，操作系统自动淘汰冷页（LRU）
```

### Page Fault 和按需加载

```
应用访问: addr[offset]
     │
     ▼
┌──────────────────────────────────────────────────────────────┐
│ CPU 查询页表: "虚拟地址 → 物理地址"                             │
└────────────────┬─────────────────────────────────────────────┘
                 │
         ┌───────┴───────┐
         │               │
         ▼               ▼
   ┌─────────┐     ┌──────────┐
   │ 页在内存 │     │ 页不在内存│ (Page Fault)
   └────┬────┘     └─────┬────┘
        │                │
        ▼                ▼
   ┌─────────┐     ┌─────────────────────┐
   │ 直接返回 │     │ 内核处理 Page Fault: │
   │ 数据    │     │                      │
   └─────────┘     │ 1. 分配物理页 (4KB)  │
                   │ 2. 从磁盘读取数据     │
                   │ 3. 更新页表映射       │
                   │ 4. 返回用户态         │
                   └──────────┬────────────┘
                              │
                              ▼
                   ┌─────────────────────┐
                   │ 物理内存不足？       │
                   │                      │
                   │ LRU 算法淘汰旧页:    │
                   │ - 选最久未用的页     │
                   │ - 脏页写回磁盘       │
                   │ - 释放物理页         │
                   └─────────────────────┘
```

### 实际案例：reth 内存使用

```bash
# 启动 reth，MDBX 200GB + Static Files 600GB
$ reth node

# 查看进程内存
$ ps aux | grep reth
USER   PID  %CPU  %MEM    VSZ      RSS
bob    1234  5.0   15.0  850000000  1200000
                          ↑          ↑
                          850GB      1.2GB
                          虚拟内存    物理内存

VSZ (Virtual Size):  850 GB
  = 程序本身 (500MB)
  + MDBX mmap (200GB)
  + Static Files mmap (600GB)
  + 其他 (50GB)

RSS (Resident Set):  1.2 GB
  = 程序代码和堆 (700MB)
  + Page cache (500MB)  ← 实际访问过的数据

# 系统内存使用
$ free -h
              total   used   free   buff/cache   available
Mem:           8.0G   2.5G   1.0G   4.5G         5.2G
                                    ↑
                              包含 MDBX/Static Files
                              的 page cache
```

**关键启示：**

```
问：8GB 内存能运行 1.4TB 数据的全节点吗？
答：可以！

原理：
  1. mmap 只占虚拟地址空间，不占物理内存
  2. 只有访问的数据才加载到物理内存（page cache）
  3. 热数据（最近区块）留在内存，冷数据按需加载
  4. 物理内存不足时，操作系统自动淘汰冷页

实际表现：
  - 查询最近区块：~100μs（数据在 page cache）
  - 查询历史区块：~5ms（需要从磁盘加载）
  - 同步新区块：高性能（状态数据都在内存）
```

---

## 关键文件位置

`Full_code.wiki.md` 对存储层的划分可以对应到当前代码里的这些入口：

| 领域 | 入口 | 作用 |
|------|------|------|
| 数据库抽象 | `crates/storage/db-api/` | 定义表、事务、游标和 DupSort 等通用接口 |
| 本地数据访问 | `crates/storage/provider/` | `ProviderFactory` 统一管理 MDBX、StaticFileProvider 和可选 RocksDB |
| 远程数据访问 | `crates/storage/rpc-provider/` | 通过外部 RPC 端点实现 provider trait，适合测试、轻量集成和 ExEx 场景 |
| Trie 通用类型 | `crates/trie/common/` | Trie 输入、节点、证明和更新集等共享类型 |
| Trie 数据库层 | `crates/trie/db/` | 数据库游标、changeset 缓存、数据库状态根和证明生成 |
| Trie 并行层 | `crates/trie/parallel/` | 并行状态根和证明计算 |
| 稀疏 Trie | `crates/trie/sparse/` | 基于 arena 的稀疏 trie，用于高效更新和证明揭示 |
| 核心 Trie 算法 | `crates/trie/trie/` | MPT 根计算、证明和 witness 生成 |

| 文件 | 行数 | 职责 |
|------|------|------|
| `crates/storage/db-api/src/tables/mod.rs` | 538 | 所有数据库表定义 |
| `crates/trie/trie/src/trie.rs` | ~1500 | StateRoot 计算核心算法 |
| `crates/trie/db/src/trie_cursor.rs` | ~300 | Trie 节点的数据库游标 |
| `crates/trie/db/src/hashed_cursor.rs` | ~200 | Hashed 数据的游标 |
| `crates/storage/provider/src/providers/static_file/manager.rs` | ~2000 | StaticFileProvider 核心 |
| `crates/storage/nippy-jar/src/lib.rs` | ~1000 | NippyJar 格式实现 |
| `crates/static-file/static-file/src/static_file_producer.rs` | ~500 | 数据迁移逻辑 |

---

## 建议阅读顺序

### 第一步：理解数据表结构

1. 读 `tables/mod.rs` 的表定义
   - PlainAccountState, PlainStorageState
   - HashedAccounts, HashedStorages
   - AccountsTrie, StoragesTrie

### 第二步：理解 State Root 计算

2. 读 `trie/trie.rs` 的 StateRoot 结构体
   - 两个 cursor factory 的作用
   - `root_with_updates()` 方法

3. 读 `trie_cursor.rs` 和 `hashed_cursor.rs`
   - 如何从数据库读取 trie 节点
   - 如何读取 hashed 数据

### 第三步：理解 Static Files

4. 读 `static_file/manager.rs` 的 StaticFileProvider
   - 六种 Segment
   - 文件组织方式

5. 读 `nippy-jar/src/lib.rs`
   - NippyJar 的列式存储原理
   - mmap 读取流程

6. 读 `static_file_producer.rs`
   - 数据迁移触发条件
   - copy_to_static_files 流程

---

## 动手实验

### 实验一：查看数据库表大小

```bash
# 数据目录位置（各平台不同）：
# macOS:   ~/Library/Application Support/reth/
# Linux:   ~/.local/share/reth/
# 以 dev 链为例：
DATADIR=~/Library/Application\ Support/reth/dev   # macOS
# DATADIR=~/.local/share/reth/dev                  # Linux

# 查看目录结构
ls -lh "$DATADIR"

# 查看 MDBX 数据库大小（文件名是 mdbx.dat，不是 data.mdb）
ls -lh "$DATADIR/db/mdbx.dat"

# 查看 Static Files 大小
ls -lh "$DATADIR/static_files/"

# 使用 reth 工具查看所有表的行数和大小
reth db --datadir "$DATADIR" --chain dev stats

# 查看某个表的内容（JSON 格式）
reth db --datadir "$DATADIR" --chain dev list PlainAccountState --json --len 5
```

### 实验二：观察 mmap 内存使用

```bash
# 启动 reth
reth node &

# 持续监控内存
watch -n 1 'ps aux | grep reth | grep -v grep'

# 观察：
# - VSZ（虚拟内存）很大（数百 GB）
# - RSS（物理内存）相对小（几 GB）
# - RSS 会随着访问数据逐渐增长
# - 达到上限后稳定（操作系统 LRU 淘汰）
```

### 实验三：手动触发 Static File 迁移

```bash
DATADIR=~/Library/Application\ Support/reth/mainnet   # macOS

# 查看当前 static file 状态
reth db --datadir "$DATADIR" stats

# 手动运行 static file 生成（注意：目前 Producer 只迁移 Receipts）
reth stage run static-file

# 观察文件变化
ls -lh "$DATADIR/static_files/"
```

### 实验四：对比查询性能

```rust
// 查询最近的区块（热数据）
let start = Instant::now();
let header = provider.header_by_number(tip_block)?;
println!("Hot data: {:?}", start.elapsed()); // ~100μs

// 查询历史区块（冷数据，可能在 static file）
let start = Instant::now();
let header = provider.header_by_number(1_000_000)?;
println!("Cold data: {:?}", start.elapsed()); // ~5ms（首次）

// 再次查询同一历史区块（已在 page cache）
let start = Instant::now();
let header = provider.header_by_number(1_000_000)?;
println!("Cached cold data: {:?}", start.elapsed()); // ~200μs
```

---

## 知识检验

完成本章学习后，你应该能回答：

- [ ] State Trie 的节点存储在哪个 MDBX 表中？
- [ ] 为什么需要 PlainAccountState、HashedAccounts、AccountsTrie 三层数据？
- [ ] 计算 state_root 时，为什么需要两个 cursor（trie + hashed）？
- [ ] 修改 100 个账户后，需要重新计算多少个 trie 节点？（提示：log(N)）
- [ ] Static Files 有哪六种 Segment？其中哪些由同步阶段直接写入，哪些由 StaticFileProducer 迁移？
- [ ] NippyJar 的列式存储和压缩带来了什么好处？
- [ ] 数据从 MDBX 迁移到 Static Files 后，MDBX 中的数据会立即删除吗？谁负责删除？
- [ ] MDBX 的 30 张表按用途分为哪几类？Bytecodes 表属于哪一类？
- [ ] mmap 是什么？它映射到虚拟地址空间还是物理内存？
- [ ] 为什么 8GB 物理内存可以 mmap 一个 200GB 的文件？
- [ ] 当物理内存不足时，操作系统如何处理 page cache？
- [ ] reth 的三层存储（MDBX + Static Files + RocksDB）各自的优势是什么？

---

## 下一章预告

**第三章：同步与共识**

- Pipeline 架构：Stages 如何协同工作？
- Headers → Bodies → Execution → Merkle 的执行顺序
- Consensus Engine 如何处理 CL 的 newPayload 和 forkchoiceUpdated？
- 如何从创世块同步到最新块？

**核心路径：**
```
crates/stages/          # Pipeline 和 Stages
crates/engine/          # Consensus Engine (Engine API)
crates/consensus/       # 共识规则验证
```
