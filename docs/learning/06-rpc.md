# 第6章：RPC 层 —— 节点如何对外提供服务

## 概述

以太坊节点通过 **JSON-RPC** 协议向外部世界暴露接口：钱包查询余额、Dapp 提交交易、区块链浏览器读取区块、出块软件（如 MEV-boost）调用 Engine API —— 全都经过这一层。

reth 的 RPC 层设计目标很清晰：

- **模块化**：每个命名空间（`eth_`、`debug_`、`trace_` 等）独立组装，按需启用
- **传输无关**：同一套 handler 可以跑在 HTTP / WebSocket / IPC 上
- **并发安全**：CPU 密集操作（trace）和 IO 密集操作（eth_call）分别用独立的信号量限流
- **可扩展**：自定义链可以替换具体实现，只需满足 trait 约束

本章覆盖：子 crate 分工 → 服务器构建流程 → 模块系统 → eth_ 命名空间 trait 体系 → 关键方法实现 → 阻塞任务管理 → 缓存层 → 中间件 → Engine API 通道。

---

## 1. 子 crate 分工

`crates/rpc/` 下有 11 个子 crate，职责分明：

| crate | 职责 |
|-------|------|
| `rpc-builder` | 主协调者：组装各模块、启动 HTTP/WS/IPC 服务器 |
| `rpc-server-types` | `RethRpcModule` 枚举、`RpcModuleSelection` 等核心类型 |
| `rpc-eth-api` | `eth_` 命名空间的 trait 定义（接口层） |
| `rpc-eth-types` | `eth_` 命名空间的配置、缓存、错误类型（数据层） |
| `rpc` | 具体实现：`EthApi`、`AdminApi`、`DebugApi`、`TraceApi` 等 |
| `rpc-layer` | Tower HTTP 中间件：JWT 鉴权、Gzip 压缩 |
| `rpc-convert` | RPC 类型 ↔ reth 内部类型的转换逻辑 |
| `rpc-engine-api` | Engine API 实现（`engine_newPayload`、`engine_forkchoiceUpdated`） |
| `ipc` | Unix domain socket IPC 传输层实现 |
| `rpc-api` | jsonrpsee `#[rpc]` 宏生成的 trait 声明（客户端+服务端） |
| `rpc-e2e-tests` / `rpc-testing-util` | 集成测试工具 |

分层关系如下：

```
外部调用者 (curl / ethers.js / MetaMask)
      │
      ▼
  传输层 (HTTP / WebSocket / IPC)
      │
      ▼
  中间件 (JWT Auth / CORS / 压缩)  ← rpc-layer
      │
      ▼
  jsonrpsee 路由
      │
      ▼
  RPC handler (EthApi / DebugApi / ...)  ← rpc + rpc-eth-api
      │
      ▼
  Provider / TransactionPool / Network  ← 节点核心组件
```

---

## 2. 模块系统：RethRpcModule 与 RpcModuleSelection

### 2.1 可用的 RPC 模块

所有 reth 支持的命名空间由 `RethRpcModule` 枚举表示
（位于 `crates/rpc/rpc-server-types/src/module.rs:299`）：

```rust
pub enum RethRpcModule {
    Admin,      // admin_   节点管理（添加 peer 等）
    Debug,      // debug_   调试（traceTransaction、storageRangeAt 等）
    Eth,        // eth_     标准以太坊 API
    Net,        // net_     网络信息（版本、peer 数）
    Trace,      // trace_   OpenEthereum 格式 trace
    Txpool,     // txpool_  交易池内容查询
    Web3,       // web3_    客户端版本、sha3
    Rpc,        // rpc_     RPC 模块信息
    Reth,       // reth_    reth 特有扩展
    Ots,        // ots_     Otterscan 扩展
    Flashbots,  // flashbots_ MEV bundle
    Miner,      // miner_   矿工接口（兼容 geth）
    Mev,        // mev_     MEV 相关
    Testing,    // testing_ 测试专用
    Other(String), // 自定义模块
}
```

### 2.2 按传输选择模块

`RpcModuleSelection` 控制每个传输层开放哪些模块：

```rust
pub enum RpcModuleSelection {
    All,                              // 开放所有模块
    Standard,                         // 默认：eth + net + web3
    Selection(HashSet<RethRpcModule>), // 自定义集合
}
```

IPC 默认开放所有模块（本地访问无需安全限制），HTTP/WS 默认只开放 Standard。

---

## 3. 服务器构建流程

### 3.1 两个关键 Builder

reth 用两个 builder 分工合作构建 RPC 服务器：

```
RpcModuleBuilder  →  "要提供哪些 API？用什么组件来支撑？"
RpcServerConfig   →  "用什么传输方式？监听哪个端口？"
```

**`RpcModuleBuilder`**（位于 `crates/rpc/rpc-builder/src/lib.rs:116`）接收节点核心组件：

```rust
pub struct RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus> {
    provider: Provider,    // 区块链状态读取
    pool: Pool,            // 交易池
    network: Network,      // P2P 网络
    executor: Box<dyn TaskSpawner>,
    evm_config: EvmConfig, // EVM 配置
    consensus: Consensus,
    _primitives: PhantomData<N>,
}
```

**`RpcServerConfig`** 配置传输层：

```rust
pub struct RpcServerConfig<RpcMiddleware = Identity> {
    http_addr: Option<SocketAddr>,
    http_cors_domains: Option<String>,
    ws_addr: Option<SocketAddr>,
    ipc_endpoint: Option<String>,
    jwt_secret: Option<JwtSecret>,   // Engine API 鉴权
    rpc_middleware: RpcMiddleware,   // Tower 中间件栈
    // ...
}
```

### 3.2 组装流程

节点启动时，典型流程如下：

```
1. RpcModuleBuilder::new(provider, pool, network, ...)
        │
        ▼
2. .build(transport_config)          ← 按传输配置创建 TransportRpcModules
        │
        ├─ 为 eth 命名空间：bootstrap_eth_api()
        │     ├─ 创建 EthApi（含 EthStateCache）
        │     ├─ 创建 EthFilter（轮询过滤器）
        │     └─ 创建 EthPubSub（订阅，WS/IPC 专用）
        │
        ├─ register_eth()   / register_debug()  / register_trace() ...
        │     └─ 将各命名空间方法插入 RpcModule
        │
        └─ 返回 TransportRpcModules { http, ws, ipc }
                │
                ▼
3. RpcServerConfig::start(&modules)  ← 启动实际网络服务器
        ├─ 若 HTTP == WS 端口 → 合并为单个 jsonrpsee 服务器
        ├─ 否则分别启动 HTTP 服务器、WS 服务器
        └─ 独立启动 IPC 服务器（Unix domain socket）
```

### 3.3 Auth 服务器与普通服务器分离

Engine API（`engine_newPayload`、`engine_forkchoiceUpdated` 等）运行在**独立的 Auth 服务器**上，通常监听 `8551` 端口：

```
普通 RPC 服务器  (8545)  ← 面向公网，无需鉴权
Auth 服务器     (8551)  ← 仅供 CL 访问，必须 JWT 鉴权
```

Auth 服务器由 `build_with_auth_server()` 构建，内含 Engine API 模块 + 特殊的 `engine_eth_*` 方法（供 CL 端查询区块用）。

---

## 4. 传输层与中间件

### 4.1 三种传输层

| 传输 | 实现 | 特点 |
|------|------|------|
| HTTP | jsonrpsee `ServerBuilder` | 请求-响应，无状态 |
| WebSocket | jsonrpsee `ServerBuilder` | 持久连接，支持订阅（`eth_subscribe`） |
| IPC | reth 自研 `IpcServer` | Unix domain socket，本地进程间，低延迟，支持订阅 |

### 4.2 中间件栈

请求经过 Tower 中间件栈（由外向内）：

```
[CORS 检查]          ← tower_http::cors::CorsLayer
[JWT 鉴权]           ← AuthLayer<JwtAuthValidator>（仅 Auth 服务器）
[Gzip/brotli 压缩]   ← CompressionLayer
[jsonrpsee RPC 路由] ← 方法分发
[handler 执行]       ← EthApi::call() 等
```

**AuthLayer**（位于 `crates/rpc/rpc-layer/src/auth_layer.rs:42`）是一个 Tower `Layer`，拦截 `Authorization` 请求头，JWT 校验失败则直接返回 401，通过则放行给内层服务。

---

## 5. eth_ 命名空间：trait 体系

`eth_` 是使用最广泛的命名空间。reth 用**分层 trait** 来组织它，而非一个巨大的实现类。

### 5.1 trait 分层结构

```
底层：Load* traits（原子数据库读取）
         │
         ▼
中层：Eth* traits（组合 Load 构建 RPC 语义）
         │
         ▼
顶层：FullEthApi（所有 Eth* trait 的聚合）
         │
         ▼
服务端：FullEthApiServer（jsonrpsee 生成的 server trait 实现）
```

**Load 层**（每一项对应一类原子读操作）：

| trait | 职责 |
|-------|------|
| `LoadBlock` | 从 DB 或缓存读取区块 |
| `LoadTransaction` | 读取交易（池 + DB） |
| `LoadReceipt` | 读取收据 |
| `LoadState` | 读取账户状态 |
| `LoadFee` | 读取 gas price、base fee |
| `LoadPendingBlock` | 构造 pending 区块环境 |

**Eth 层**（按 RPC 方法语义分类，位于 `crates/rpc/rpc-eth-api/src/helpers/mod.rs:53`）：

```rust
pub trait FullEthApi:
    FullEthApiTypes
    + EthApiSpec        // eth_chainId, eth_syncing, eth_blockNumber
    + EthTransactions   // eth_sendRawTransaction, eth_getTransactionByHash
    + EthBlocks         // eth_getBlockByHash, eth_getBlockByNumber
    + EthState          // eth_getBalance, eth_getCode, eth_getStorageAt
    + EthCall           // eth_call, eth_estimateGas, eth_createAccessList
    + EthFees           // eth_gasPrice, eth_feeHistory, eth_maxPriorityFeePerGas
    + Trace             // trace_* 和 debug_trace* 的底层支持
    + LoadReceipt
{}
```

### 5.2 jsonrpsee 宏生成的 server trait

`EthApi` trait（位于 `crates/rpc/rpc-eth-api/src/core.rs:56`）由 `#[rpc]` 宏标注：

```rust
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
pub trait EthApi<TxReq, T, B, R, H, RawTx> {
    #[method(name = "protocolVersion")]
    async fn protocol_version(&self) -> RpcResult<U64>;

    #[method(name = "syncing")]
    fn syncing(&self) -> RpcResult<SyncStatus>;

    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, bytes: Bytes) -> RpcResult<B256>;

    #[method(name = "call")]
    async fn call(&self, request: TxReq, block_id: Option<BlockId>, ...) -> RpcResult<Bytes>;
    // ...
}
```

宏展开后生成 `EthApiServer` trait（方法名自动加 `eth_` 前缀），并为任何实现了 `FullEthApi` 的类型自动实现它。

### 5.3 具体实现：EthApi 结构体

位于 `crates/rpc/rpc/src/eth/`，`EthApi<NC, RpcConv>` 是 reth 的默认 L1 实现：

```rust
pub struct EthApi<NC, RpcConv> {
    inner: Arc<EthApiInner<NC, RpcConv>>,
}

struct EthApiInner<NC, RpcConv> {
    provider: NC::Provider,     // 区块链读取
    pool: NC::Pool,             // 交易池
    network: NC::Network,       // 网络信息
    evm_config: NC::Evm,        // EVM 执行配置

    converter: RpcConv,         // 内部类型 → RPC 类型转换
    cache: EthStateCache<...>,  // 异步 LRU 缓存服务
    config: EthConfig,          // gas cap、并发限制等
}
```

`EthApi` 是 `Arc` 包裹的，clone 开销极低，可以安全地在多个 handler 间共享。

---

## 6. 三个关键方法的实现路径

### 6.1 eth_sendRawTransaction

客户端广播一笔交易的入口（位于 `crates/rpc/rpc-eth-api/src/helpers/transaction.rs:79`）：

```
send_raw_transaction(bytes)
    ↓
recover_raw_transaction()    // 解码 RLP，恢复签名者地址
    ↓
send_transaction(WithEncoded<Recovered<Tx>>)
    ↓
pool.add_transaction(External, tx)   // 插入交易池，标记为 External 来源
    ↓
返回 TxHash
```

注意几点：
- **不是 blocking 操作**：交易池插入非常快，直接在 async 上下文执行，无需 `spawn_blocking`
- **来源标记为 `External`**：即使通过本地 RPC 提交，也会被 P2P 广播
- **sidecar 处理**：EIP-4844 blob 交易的 sidecar 会存入 `BlobStore`，不跟随交易本身进入池

### 6.2 eth_call（EVM 调用，不上链）

这是最复杂的方法之一，核心挑战是 EVM 执行是 CPU 密集型的，不能阻塞 async runtime：

```
call(request, block_id, state_override)
    ↓
acquire_owned_blocking_io()     // 获取信号量许可（限制并发）
    ↓
spawn_blocking_io(|api| {       // 移交 tokio blocking 线程池
    provider.state_provider(block_id)?  // 从 DB 创建状态视图
        ↓
    构造 EVM 环境（CfgEnv, BlockEnv, TxEnv）
        ↓
    evm.transact(call_env)?     // 执行（不写入状态）
        ↓
    返回 output bytes
})
    ↓
释放信号量许可
    ↓
返回 RpcResult<Bytes>
```

重要细节：`eth_call` 和 `eth_estimateGas` 会**禁用 base fee 检查**（与 geth 行为一致）——即使 `maxFeePerGas` 低于当前 base fee，模拟执行仍然可以成功。这对调试非常有用。

### 6.3 eth_getTransactionByHash

先查池（pending），再查 DB（已打包）：

```
get_transaction_by_hash(hash)
    ↓
pool.get(hash)               // 先检查 pending 池
    │
    ├─ Some(pooled_tx) → 转换为 RPC 类型，标记 blockHash=null（pending）
    │
    └─ None → cache.get_transaction(hash)  // 查询 EthStateCache
                    ↓
               provider.transaction_by_hash(hash)  // 从 DB 读取
                    ↓
               Some(tx) → 补充 TransactionInfo（block_hash、block_number、index）
               None → 返回 null
```

---

## 7. 阻塞任务管理：SpawnBlocking trait

EVM 执行、状态读取等操作不能占用 tokio 的 async 线程，reth 通过 `SpawnBlocking` trait（位于 `crates/rpc/rpc-eth-api/src/helpers/blocking_task.rs:26`）统一管理这些资源。

### 7.1 两类任务池

```
               请求
                │
    ┌───────────┴────────────┐
    │                        │
blocking IO 池            Tracing 池
(eth_call, estimate...)   (debug_traceTransaction...)
    │                        │
tokio::spawn_blocking     rayon 线程池
    │                        │
Arc<Semaphore>            BlockingTaskGuard(Semaphore)
(许可数较多，如 256)       (许可数少，如 10)
```

**为什么分两类？**

Tracing 类请求（`debug_traceTransaction`、`trace_call`）：
- 需要重放历史区块的所有交易
- CPU 占用高，内存积累大（trace 结果可达数 MB）
- 用 rayon 线程池（CPU 并行更高效）
- 信号量许可数少，严格限制并发

IO 类请求（`eth_call`、`eth_estimateGas`）：
- 单次 EVM 执行，通常很快
- 混合 IO + CPU，用 tokio blocking 线程池
- 信号量许可数多，允许更高并发

### 7.2 加权信号量

对于消耗资源可变的操作（如按区块范围查询 fee history），reth 使用加权信号量：

```rust
fn acquire_weighted_blocking_io(&self, weight: u32)
    -> impl Future<Output = Result<OwnedSemaphorePermit, AcquireError>>
{
    let total_permits = guard.available_permits().max(1) as u32;
    // weight=10 意味着"这个请求相当于 10 个普通请求"
    // 一次获取 total/weight 个许可，从而限制该类型请求的最大并发数
    let permits_to_acquire = (total_permits / weight).max(1);
    guard.acquire_many_owned(permits_to_acquire)
}
```

`weight` 越大，一次消耗的许可越多，同类并发请求越少。这使得不同"重量"的请求能在同一个信号量池上公平竞争资源。

---

## 8. EthStateCache：异步 LRU 缓存服务

频繁读取的数据（区块、收据、Header）通过 `EthStateCache` 缓存，避免每次都走 DB（mmap 读取虽然快，但仍有开销）。

### 8.1 架构：Actor 模式

`EthStateCache` 不是一个简单的 `HashMap`，而是一个**独立任务上运行的 actor**：

```
EthStateCache（前端，Clone 廉价）
    │  unbounded channel (CacheAction)
    ▼
CacheService（后台任务，单线程）
    ├─ BlockLruCache：LRU<B256, Arc<RecoveredBlock<B>>>
    ├─ ReceiptsLruCache：LRU<B256, Arc<Vec<R>>>
    └─ HeaderLruCache：LRU<B256, Header>
```

多个 RPC handler 持有 `EthStateCache` 克隆，通过 channel 发送请求；后台的 `CacheService` 负责实际的 DB 读取和 LRU 管理。

### 8.2 MultiConsumerLruCache：合并重复请求

若两个请求同时查询同一个区块，`MultiConsumerLruCache` 会：
1. 第一个请求到达 → 发起 DB 读取，记录其 `oneshot::Sender`
2. 第二个请求到达 → 发现 DB 读取已在进行，将其 `Sender` 排队
3. DB 返回结果 → 一次广播给所有等待者

这避免了对同一区块的重复 DB 读取，在高并发场景下效果显著。

---

## 9. 类型转换：RPC 类型 ↔ 内部类型

reth 内部使用自定义的 `SignedTransaction`、`Block` 等类型，而 JSON-RPC 响应需要 alloy 定义的标准 RPC 类型。`rpc-convert` crate 负责这个转换。

```
内部类型 (reth primitives)          RPC 类型 (alloy rpc-types)
─────────────────────────          ─────────────────────────
SignedTransaction           →      Transaction { from, to, value, ... }
Block<Header>               →      Block { hash, number, transactions, ... }
Receipt                     →      TransactionReceipt { status, logs, gasUsed, ... }
```

转换由 `RpcConverter<N, EvmConfig, R>` 承担，通过 `RpcConvert` trait 提供接口，允许不同的链（L2）注入自定义的转换逻辑。

---

## 10. 订阅与 PubSub

`eth_subscribe` 和 `eth_unsubscribe` 是 WebSocket/IPC 专有的实时推送接口，由 `EthPubSub` 处理。

支持的订阅类型：

| 订阅 | 触发时机 | 内容 |
|------|---------|------|
| `newHeads` | 每次出新块 | 新块的 Header |
| `logs` | 每次出新块 | 符合 filter 条件的 logs |
| `newPendingTransactions` | 新交易进入 pending 池 | 交易 hash（或完整交易） |
| `syncing` | 节点同步状态变化 | `SyncStatus` |

实现机制：`EthPubSub` 订阅 `CanonStateNotification`（规范链状态变更事件），在 tokio 任务中将事件转换为 jsonrpsee 订阅消息推送给客户端。

---

## 11. Engine API：CL 与 EL 的专用通道

Engine API 是共识层（CL，如 Lighthouse）和执行层（EL，reth）之间的通信协议，定义在 EIP-3675 中。

它运行在独立的 Auth 服务器上（默认端口 8551），所有请求必须携带有效的 JWT token。

核心方法（位于 `crates/rpc/rpc-engine-api/`）：

| 方法 | 调用时机 | 作用 |
|------|---------|------|
| `engine_newPayloadV*` | CL 收到新区块 | 将区块发给 EL 执行验证 |
| `engine_forkchoiceUpdatedV*` | 链头变化 | 通知 EL 当前规范链头，可附带出块指令 |
| `engine_getPayloadV*` | CL 需要出块 | EL 返回构建好的 payload |
| `engine_getBlobsV*` | CL 需要 blob | EL 从 BlobStore 返回 blob sidecar |

这些方法的处理流程在[第4章](04-engine-api-and-consensus.md)中已详细介绍。

---

## 12. 完整请求路径：以 eth_call 为例

从客户端发起请求到返回结果的完整路径：

```
① 客户端
   POST http://localhost:8545
   {"jsonrpc":"2.0","method":"eth_call","params":[{...}, "latest"],"id":1}

② 网络层（Tokio TCP）
   接收 TCP 连接

③ HTTP 中间件栈
   CORS：检查 Origin 头
   压缩：检查 Accept-Encoding，响应可选 gzip

④ jsonrpsee 路由
   解析 JSON-RPC，找到 "eth_call" 对应的 handler

⑤ EthApi::call()（async）
   - 解析 CallRequest 参数
   - 获取目标 block_id（"latest" → 当前链头）
   - acquire_owned_blocking_io()：等待信号量许可

⑥ spawn_blocking_io（tokio blocking 线程池）
   - provider.state_provider(block_id)：创建该块的状态快照
   - 构造 CfgEnv（disable_base_fee=true，chain_id 等）
   - 构造 BlockEnv（coinbase、basefee、gas_limit 等）
   - 构造 TxEnv（from、to、data、value 等）
   - evm.transact()：Revm 执行交易（不写入状态）
   - 提取 ExecutionResult

⑦ 返回 async 上下文
   - 释放信号量许可
   - RpcConverter 将结果转换为 Bytes

⑧ jsonrpsee 响应序列化
   {"jsonrpc":"2.0","result":"0x...","id":1}

⑨ 客户端收到响应
```

整个过程中，async runtime 仅在步骤 ④⑤⑦⑧ 中占用线程；EVM 执行（步骤 ⑥）在独立的 blocking 线程池中进行，不阻塞其他请求的处理。

---

## 13. 关键设计决策

**为什么 eth_ 命名空间用 trait 分层，而不是一个大结构体？**

这样 L2 链（OP Stack、Scroll 等）可以只覆盖需要差异化的部分。例如，OP Stack 的 `eth_getBlockByHash` 需要包含额外的 L1 信息字段，只需覆盖 `EthBlocks` 中的对应方法，其他方法复用 L1 实现。

**为什么 EthStateCache 用 Actor 模式而不是 `Mutex<LruMap>`？**

`Mutex<LruMap>` 有两个问题：
1. 持锁期间的 DB IO 会阻塞所有等待者
2. 无法合并对同一 key 的重复请求

Actor 模式将 DB 访问集中在单一任务，天然无锁，且 `MultiConsumerLruCache` 能把并发的相同请求合并为一次 DB 查询。

**两个信号量池的必要性**

若 tracing 请求（可能几秒钟）与 eth_call（通常 <10ms）共用一个池，少量慢请求会耗尽许可，让大量快请求排队等待。分开之后，各自的 SLA 互不干扰。

---

## 小结

| 组件 | 作用 |
|------|------|
| `RpcModuleBuilder` | 组装 RPC 模块，连接节点组件 |
| `RpcServerConfig` | 配置传输层，启动实际服务器 |
| `RethRpcModule` | 命名空间枚举，按传输独立启用 |
| `FullEthApi` | eth_ 所有功能的 trait 聚合 |
| `SpawnBlocking` | 两类任务池（IO / Tracing）+ 信号量限流 |
| `EthStateCache` | Actor 模式 LRU 缓存，合并重复请求 |
| `RpcConverter` | 内部类型 → RPC 类型转换，链可扩展 |
| `AuthLayer` | Tower 中间件，JWT 鉴权（Engine API） |
| Engine API | CL ↔ EL 专用通道，独立 8551 端口 |

RPC 层是节点对外的"门面"，设计上追求在高并发下对 async runtime 的最小侵占：所有阻塞操作（EVM 执行、DB 读取）都被隔离在专用线程池里，async 线程只负责路由、序列化和结果传递。

---

**下一章：[第7章：网络层与 P2P 协议](07-networking.md)**

---

## 附录：关键文件速查

| 内容 | 文件 |
|------|------|
| RpcModuleBuilder 主体 | `crates/rpc/rpc-builder/src/lib.rs:116` |
| RethRpcModule 枚举 | `crates/rpc/rpc-server-types/src/module.rs:299` |
| EthApi trait（rpc 宏） | `crates/rpc/rpc-eth-api/src/core.rs:56` |
| FullEthApi 聚合 trait | `crates/rpc/rpc-eth-api/src/helpers/mod.rs:53` |
| SpawnBlocking trait | `crates/rpc/rpc-eth-api/src/helpers/blocking_task.rs:26` |
| EthStateCache 前端 | `crates/rpc/rpc-eth-types/src/cache/mod.rs:67` |
| AuthLayer（JWT） | `crates/rpc/rpc-layer/src/auth_layer.rs:42` |
| EthTransactions trait | `crates/rpc/rpc-eth-api/src/helpers/transaction.rs:62` |
| EthHandlers 组装 | `crates/rpc/rpc-builder/src/eth.rs` |
| Engine API 实现 | `crates/rpc/rpc-engine-api/` |
