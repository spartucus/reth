# 第一章：交易执行流程

> 核心问题：当你调用 `contract.transfer()` 时，节点内部发生了什么？

## 前置知识映射

连接你已知的合约知识到 reth 底层类型：

| 合约中的概念 | reth 底层对应 |
|------------|-------------|
| `msg.sender` | `tx.recover_signer()` |
| `block.number` | `header.number` |
| `block.timestamp` | `block_env.timestamp` |
| `block.basefee` | `block_env.basefee` |
| `emit Event(...)` | `Log { address, topics, data }` |
| 交易是否 revert | `ExecutionResult::is_success()` |
| `gasleft()` | `gas_remaining` |

---

## 核心架构：两个场景

执行交易在 reth 中有两种场景：

| 场景 | 用途 | 入口 |
|------|------|------|
| **执行已有块** | 同步历史区块时验证 | `Executor::execute_one()` |
| **构建新块** | 作为验证者出块时 | `BlockBuilder::execute_transaction()` |

---

## 完整调用链

### 场景一：执行已有区块（同步/验证）

```
Executor::execute_one(block)
  │  crates/evm/evm/src/execute.rs:552
  ↓
ConfigureEvm::executor_for_block(db, block)
  │  crates/evm/evm/src/lib.rs:327
  │  ┌─ 生成 EVM 环境（block_env: gas_limit, timestamp, basefee...）
  │  └─ 生成 cfg_env（激活的 hardfork 规则）
  ↓
BlockExecutor::execute_block(txs)      ← 来自 alloy-evm
  │  对每笔交易：
  │  ├─ 计算 intrinsic gas（基础消耗，21000 起步）
  │  ├─ 执行 EVM 指令序列
  │  └─ RethReceiptBuilder::build_receipt()
  │       crates/ethereum/evm/src/receipt.rs:16
  ↓
BlockExecutionResult {
    receipts: Vec<Receipt>,
    gas_used: u64,
    blob_gas_used: u64,   // EIP-4844 blob 数据的 gas
    requests: Requests,   // EIP-7685 执行层请求
}
```

### 场景二：构建新区块（验证者出块）

在场景一的基础上，还需要额外组装完整的块头：

```
BasicBlockBuilder::finish()
  │  crates/evm/evm/src/execute.rs:477
  │  ├─ 计算 state_root（执行后的 Merkle Patricia Trie 根）
  ↓
EthBlockAssembler::assemble_block()
     crates/ethereum/evm/src/build.rs:39
     ├─ 计算 transactions_root（交易列表的 MPT 根）
     ├─ 计算 receipts_root（receipts 列表的 MPT 根）
     ├─ 计算 logs_bloom（所有 logs 的布隆过滤器）
     ├─ 填充 gas_used、blob_gas_used 等
     └─ 返回完整的 Block { header, body }
```

---

## 关键数据结构

### Receipt（交易回执）

**文件：** `crates/ethereum/evm/src/receipt.rs`（仅 27 行）

```rust
Receipt {
    tx_type: TxType,           // Legacy / EIP-2930 / EIP-1559 / EIP-4844
    success: bool,             // false 表示合约 revert 了
    cumulative_gas_used: u64,  // 注意：是累计值，不是单笔 gas
    logs: Vec<Log>,            // emit 出来的所有事件
}
```

> **注意 cumulative_gas_used**：这是该块中**截止该笔交易**的 gas 总消耗，不是单笔交易的。
> 若想得到单笔 gas，需要用当前 receipt 的值减去上一笔 receipt 的值。

### BlockExecutionOutput（块执行输出）

**文件：** `crates/evm/execution-types/src/execute.rs`（61 行）

```rust
BlockExecutionOutput {
    result: BlockExecutionResult<Receipt>,  // receipts + gas 统计
    state: BundleState,                     // 所有状态变化（账户余额、存储、代码）
}
```

### ExecutionOutcome（多块聚合结果）

**文件：** `crates/evm/execution-types/src/execution_outcome.rs`

```rust
ExecutionOutcome {
    bundle: BundleState,          // 多块聚合后的所有状态变化
    receipts: Vec<Vec<Receipt>>,  // 按块分组的 receipts
    first_block: BlockNumber,     // 起始块号
    requests: Vec<Requests>,      // 每块的 EIP-7685 requests
}
```

### 块头中的 Merkle 根字段

每个块头包含四个 Merkle Patricia Trie 的根哈希，全部在 `assemble_block()` 中计算：

```
Block Header
├── state_root           ← 全局账户状态树（执行完所有交易后的整体状态）
│   └── 每个合约账户下有 storage_root（该合约所有 slot 的子树）
├── transactions_root    ← 本块交易列表树（tx_index → 交易数据）
├── receipts_root        ← 本块回执树（tx_index → Receipt）
└── withdrawals_root     ← 本块提款列表树（Shanghai 后新增）
```

**计算位置对比：**

| 根字段 | 计算位置 | 原因 |
|--------|---------|------|
| `state_root` | `finish()` 内，`state_root_with_updates()` | 需要对比执行前后的状态差异（BundleState），计算最重 |
| `transactions_root` | `assemble_block()` | 只需遍历交易列表，与执行结果无关 |
| `receipts_root` | `assemble_block()` | 执行完成后才有 receipts，与 state_root 独立 |
| `logs_bloom` | `assemble_block()` | 从所有 Receipt 的 logs 聚合，轻量 |

```rust
// crates/ethereum/evm/src/build.rs:55-59
let transactions_root = proofs::calculate_transaction_root(&transactions);
let receipts_root = calculate_receipt_root(
    &receipts.iter().map(|r| r.with_bloom_ref()).collect::<Vec<_>>(),
);
let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));
// state_root 已由外部传入（在 finish() 中计算）
```

> **验证时的作用：** 其他节点收到新块后，会重新执行所有交易，自行计算 `state_root` 和 `receipts_root`，与块头对比。任何一个不匹配，块就是非法的。

---

## 关键文件一览

| 文件 | 行数 | 职责 | 建议阅读优先级 |
|------|------|------|-------------|
| `crates/ethereum/evm/src/receipt.rs` | 27 | Receipt 生成 | ⭐ 第一个读 |
| `crates/evm/execution-types/src/execute.rs` | 61 | 执行输出类型定义 | ⭐ 第二个读 |
| `crates/evm/evm/src/execute.rs` | 806 | 执行器 trait + 实现 | ⭐⭐ 重点 |
| `crates/evm/evm/src/lib.rs` | 585 | ConfigureEvm trait | ⭐⭐ 重点 |
| `crates/ethereum/evm/src/lib.rs` | 492 | Ethereum 特定配置 | ⭐⭐ |
| `crates/ethereum/evm/src/build.rs` | 122 | 块头组装 | ⭐ |
| `crates/evm/execution-types/src/execution_outcome.rs` | 1021 | 多块聚合结果 | 后续再看 |

---

## 建议阅读顺序

### 第一步：理解输出（从结果往回看）

1. 读 `receipt.rs`（27行）
   - 理解一笔交易执行完产出什么
   - 注意 `result.is_success()` 和 `result.into_logs()`

2. 读 `execution-types/execute.rs`（61行）
   - 理解 `BlockExecutionResult` 和 `BlockExecutionOutput` 的区别
   - `BlockExecutionOutput` 多了 `state: BundleState`，包含所有状态变化

### 第二步：理解执行器（核心流程）

3. 读 `execute.rs` 的以下部分：
   - `Executor` trait（31-151行）：接口定义
   - `BasicBlockExecutor::execute_one()` 实现（552-566行）：最核心的入口
   - `BlockBuilder` trait（315-384行）：区块构建接口

4. 读 `lib.rs (evm)` 的以下部分：
   - `ConfigureEvm` trait 文档注释
   - `executor_for_block()` 方法（327-335行）

### 第三步：理解 Ethereum 特定逻辑

5. 读 `lib.rs (ethereum/evm)`：
   - `EthEvmConfig` 结构体
   - `evm_env()` 方法：理解 block_env 如何从区块头生成

6. 读 `build.rs`：
   - 块头各字段如何计算（state_root、receipts_root、logs_bloom 等）

---

## Hardfork 与 EVM 规则

`evm_env()` 的关键职责是根据块号选择正确的 EVM 规则集（spec）：

```
规则集演进时间线（简化）：
  Frontier → Homestead → Tangerine Whistle → ... → London → Shanghai → Cancun → Prague

每个 fork 会改变：
  - 操作码的 Gas 价格
  - 新增操作码（如 PUSH0 in Shanghai）
  - 状态规则（如 EIP-1559 base fee burn in London）
  - 新特性（如 blob 交易 in Cancun）
```

在 reth 中，hardfork 激活通过 `ChainSpec` 管理，执行时通过 `SpecId` 传入 EVM。

---

## 动手实验

### 实验一：在 Receipt 生成处添加调试日志

在 `crates/ethereum/evm/src/receipt.rs` 的 `build_receipt` 中添加：

```rust
fn build_receipt<E: Evm>(&self, ctx: ReceiptBuilderCtx<'_, TxType, E>) -> Self::Receipt {
    let ReceiptBuilderCtx { tx_type, result, cumulative_gas_used, .. } = ctx;

    // 添加这一行观察每笔交易的执行结果
    tracing::debug!(
        target: "my-learning",
        tx_type = ?tx_type,
        success = result.is_success(),
        cumulative_gas = cumulative_gas_used,
        "receipt built"
    );

    Receipt {
        tx_type,
        success: result.is_success(),
        cumulative_gas_used,
        logs: result.into_logs(),
    }
}
```

启动开发节点观察日志：
```bash
RUST_LOG="my-learning=debug" cargo run --release -- node --dev
```

### 实验二：追踪一笔合约调用

用 `cast`（Foundry 工具）发起一笔交易，然后用 `eth_getTransactionReceipt` 查看结果，对应到代码中的 `Receipt` 结构体：

```bash
# 发起交易
cast send --dev <contract_addr> "transfer(address,uint256)" <to> 100

# 查看 receipt
cast receipt <tx_hash>
```

### 实验三：思考题

**问：** 一个合约调用失败（revert）时，Gas 还会被扣掉吗？

**答案提示：** 在 `receipt.rs` 里，`success = result.is_success()`，但 `cumulative_gas_used` 是始终更新的。这意味着：
- revert 时，状态变化被回滚（不影响合约存储）
- 但 gas **仍然被扣除**（已消耗的计算不能退还）
- 未使用的 gas 会被退还（gas_limit - gas_used 部分）

---

## 知识检验

完成本章学习后，你应该能回答：

- [ ] `Receipt` 中的 `cumulative_gas_used` 和单笔交易 gas 消耗有什么区别？
- [ ] 为什么 `BlockExecutionOutput` 里有 `state: BundleState`？它记录了什么？
- [ ] `Executor` 和 `BlockBuilder` 两个 trait 的使用场景分别是什么？
- [ ] EVM 的 `block_env` 包含哪些字段？这些字段和合约里的 `block.xxx` 全局变量对应吗？
- [ ] Hardfork 是如何影响交易执行的？
- [ ] `state_root` 为什么不在 `assemble_block()` 里和 `transactions_root` 一起计算？
- [ ] 一个节点如何验证收到的块的 `receipts_root` 是否正确？

---

## 下一章

**[第二章：存储架构](./02-storage-architecture.md)**

- 合约的 `storage[key] = value` 最终存到哪里？
- State Trie 和 Storage Trie 如何存储？
- `state_root` 如何计算？（增量更新原理）
- Static Files：冷数据归档方案
- Memory-mapped I/O：为什么 8GB 内存可以访问 200GB 文件？

**核心路径：**
```
crates/storage/db-api/      # 数据库表定义
crates/trie/                # MPT 实现和 state_root 计算
crates/storage/provider/    # StaticFileProvider
crates/storage/nippy-jar/   # NippyJar 文件格式
```
