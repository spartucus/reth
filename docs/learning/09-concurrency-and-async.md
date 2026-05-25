# 第9章：并发与异步

> **核心问题：** reth 如何利用多核和异步 I/O 达到高性能？各种并发模式分别用在哪里？

---

## 目录

1. [为什么并发设计如此重要？](#1-为什么并发设计如此重要)
2. [Tokio 运行时配置](#2-tokio-运行时配置)
3. [Runtime / TaskManager 体系](#3-runtime--taskmanager-体系)
4. [两类任务：普通任务 vs 关键任务](#4-两类任务普通任务-vs-关键任务)
5. [Shutdown 机制：Signal / Shutdown / GracefulShutdown](#5-shutdown-机制signal--shutdown--gracefulshutdown)
6. [两类阻塞工作：tokio blocking pool vs rayon pool](#6-两类阻塞工作tokio-blocking-pool-vs-rayon-pool)
7. [BlockingTaskPool 与 BlockingTaskGuard](#7-blockingtaskpool-与-blockingtaskguard)
8. [Channel 选型：何时用哪种 channel？](#8-channel-选型何时用哪种-channel)
9. [锁策略：parking_lot vs tokio sync](#9-锁策略parking_lot-vs-tokio-sync)
10. [tokio::select! 的使用模式](#10-tokioselect-的使用模式)
11. [并行状态根：深度案例](#11-并行状态根深度案例)
12. [一次完整的并行状态根计算流程](#12-一次完整的并行状态根计算流程)
13. [关键设计决策](#13-关键设计决策)

---

## 1. 为什么并发设计如此重要？

reth 的工作负载同时包含多种截然不同的任务类型：

| 任务类型 | 典型例子 | 特征 |
|----------|---------|------|
| **异步 I/O** | 等待网络包、等待数据库读 | 大量等待，线程绝大部分时间空闲 |
| **CPU 密集** | 状态根计算、keccak 哈希、EVM 执行 | 长时间占用 CPU，不会主动让出 |
| **混合型** | eth_call（读 DB + EVM 执行）| 两者都有 |
| **长期后台** | NetworkManager、交易池维护 | 一直运行，不能阻塞其他任务 |

如果把这些任务混在同一个线程池里，CPU 密集型任务会独占线程，导致异步 I/O 任务无法得到调度，整个系统响应迟缓。reth 的解决方案是**多层并发体系**：

```
Tokio 异步运行时（多线程，处理 I/O 和轻量任务）
    ├── tokio blocking pool（处理"需要 block_on 的阻塞 I/O"）
    └── rayon thread pool（专门处理 CPU 密集计算）
```

---

## 2. Tokio 运行时配置

整个 reth 运行在一个 tokio 多线程运行时上。当前任务系统把 tokio 配置封装到 `crates/tasks/src/runtime.rs` 的 `TokioConfig` / `RuntimeBuilder` 中；CLI runner 负责创建并进入这个运行时。默认 tokio 配置的核心常量是 `DEFAULT_THREAD_KEEP_ALIVE = 15s`：

```rust
pub const DEFAULT_THREAD_KEEP_ALIVE: Duration = Duration::from_secs(15);

pub enum TokioConfig {
    Owned {
        worker_threads: Option<usize>,
        thread_keep_alive: Duration,
        thread_name: &'static str,
    },
    ExistingHandle(Handle),
}
```

### 关键配置解析

**`new_multi_thread()`**：启用多线程调度器，工作线程数默认等于 CPU 核心数。

**`enable_all()`**：同时启用 I/O driver（epoll/kqueue）和 timer driver，支持网络 I/O 和超时。

**`thread_keep_alive(Duration::from_secs(15))`**：这是一个针对以太坊的特定优化：

```
以太坊 slot 时间 = 12 秒

每个 slot 都会有：
  - 大量网络 I/O（新区块/交易广播）
  - 状态根计算（CPU 密集）
  - 数据库写入（I/O）

如果 keep_alive < 12 秒：
  上一个 block 的工作线程退出 → 下一个 block 重新创建 → 线程创建开销重复发生

keep_alive = 15 秒（> 12 秒 slot）：
  线程在 slot 结束后短暂休眠，下个 slot 来临时直接复用 → 零线程创建开销
```

这个 15 秒的设计现在集中在 `crates/tasks/src/runtime.rs`，所有通过 `RuntimeBuilder` 创建的 owned tokio runtime 都会继承这套默认配置。

---

## 3. Runtime / TaskManager 体系

reth 没有在业务代码里到处直接使用 `tokio::task::spawn`，而是通过 `crates/tasks` 提供的 `Runtime` 统一管理异步任务、critical task panic、graceful shutdown 和 rayon 线程池。`TaskExecutor` 现在是 `Runtime` 的类型别名：

```rust
pub type TaskExecutor = Runtime;
```

### 三层结构

```
RuntimeBuilder
    └── Runtime（可 clone 的统一执行句柄）
          ├── tokio Handle
          ├── TaskManager（监控 critical panic + graceful shutdown）
          ├── cpu_pool / rpc_pool / storage_pool（rayon，feature=rayon）
          └── BlockingTaskGuard（RPC tracing 等重任务限流）
```

### TaskManager

```rust
pub struct TaskManager {
    task_events_rx: UnboundedReceiver<TaskEvent>,// 任务事件接收端
    signal: Option<Signal>,                      // 关机信号（drop 时自动触发）
    graceful_tasks: Arc<AtomicUsize>,            // 正在优雅关机的任务计数
}
```

`TaskManager` 本身是一个 `Future`——它无限等待任务事件，一旦收到 `TaskEvent::Panic`，立即返回 `Err(PanickedTaskError)` 通知主程序有关键任务崩溃了：

```rust
impl Future for TaskManager {
    type Output = Result<(), PanickedTaskError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match ready!(self.task_events_rx.poll_recv(cx)) {
            Some(TaskEvent::Panic(err)) => Poll::Ready(Err(err)),
            Some(TaskEvent::GracefulShutdown) | None => {
                if let Some(signal) = self.signal.take() {
                    signal.fire();
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}
```

### Runtime

`Runtime` 是可 clone 的任务句柄，每个需要派生任务的组件都持有一份。它既能把 future 派发到 tokio，也能把同步 CPU/IO 工作派发到专用 rayon 池：

```rust
pub struct Runtime(Arc<RuntimeInner>);
```

常用方法：
- `spawn_task(fut)`：普通 tokio 任务，响应 shutdown。
- `spawn_blocking_task(fut)`：把 future 放到 tokio blocking pool 中 `block_on`。
- `spawn_blocking(func)`：直接使用 tokio blocking pool 执行闭包。
- `spawn_blocking_named(name, func)` / `try_spawn_blocking_named`：使用 named rayon worker pool 执行闭包。
- `spawn_blocking_named_or_tokio(name, func)`：优先使用已有 named rayon pool，找不到时回退到 tokio blocking pool。
- `spawn_critical_task(name, fut)`：critical tokio task，panic 会上报给 `TaskManager`。
- `spawn_critical_blocking_task(name, fut)`：critical blocking future。
- `spawn_critical_os_thread(name, func)`：启动受监控的 OS thread。

`Runtime::current()` 可以从当前 tokio context 取得全局 runtime 句柄；`Runtime::builder()` / `RuntimeBuilder` 负责配置 tokio 和 rayon。

---

## 4. 两类任务：普通任务 vs 关键任务

### 普通任务（`spawn_task` / `spawn_blocking_task`）

```rust
// 内部实现（lib.rs:438）
async move {
    let _counter = IncCounterOnDrop::new(finished_counter); // 完成时自动计数
    let fut = pin!(fut);
    let _ = select(on_shutdown, fut).await;  // 关键：任一完成即结束
}
```

普通任务被包裹在 `select(on_shutdown, fut)` 中——当关机信号到来时，任务会立即停止（即使原来的 future 还没完成）。这是"快速停止"模式。

### 关键任务（`spawn_critical_task` / `spawn_critical_blocking_task`）

```rust
// 内部实现（runtime.rs）
let task = std::panic::AssertUnwindSafe(fut)
    .catch_unwind()
    .map_err(move |error| {
        let task_error = PanickedTaskError::new(name, error);
        error!("{task_error}");
        let _ = panicked_tasks_tx.send(TaskEvent::Panic(task_error)); // 通知 TaskManager
    });
```

关键任务额外用 `catch_unwind` 包裹，panic 不会沉默，而是发送给 `TaskManager`，进而终止整个程序。

**关键任务的使用场景**：
- 网络层：`NetworkManager`（崩溃 = 节点与网络断开）
- 引擎层：consensus engine / `ChainOrchestrator`（崩溃 = 无法处理新区块）
- 持久化层：数据库写入任务

### 带优雅关机的任务（`spawn_critical_with_graceful_shutdown_signal`）

```rust
executor.spawn_critical_with_graceful_shutdown_signal("grace", |shutdown| async move {
    // 等待关机信号，拿到 guard
    let guard = shutdown.await;
    // 做善后工作（flush 缓冲区、关闭连接...）
    flush_pending_data().await;
    // guard drop → 通知 TaskManager 我已完成
    drop(guard);
});
```

这类任务在关机时不会被强制停止，`TaskManager` 会等到所有持有 `GracefulShutdownGuard` 的任务都 drop 完 guard 后才真正退出。

---

## 5. Shutdown 机制：Signal / Shutdown / GracefulShutdown

关机机制的全貌（[crates/tasks/src/shutdown.rs](../../crates/tasks/src/shutdown.rs)）：

```
TaskManager 创建时：
  let (signal, on_shutdown) = signal();
      │                          │
      ▼                          ▼
  Signal                      Shutdown
  (oneshot::Sender<()>)       (Shared<oneshot::Receiver<()>>)
  持有在 TaskManager           可无限 clone，分发给所有任务
```

### 三个关键类型

**`Signal`（`shutdown.rs:91`）**

```rust
pub struct Signal(oneshot::Sender<()>);

impl Signal {
    pub fn fire(self) { let _ = self.0.send(()); }
}
// Drop 时自动触发（sender 关闭 → receiver 收到关闭信号）
```

**`Shutdown`（`shutdown.rs:74`）**

```rust
pub struct Shutdown(Shared<oneshot::Receiver<()>>);
// Shared = futures_util 的包装，允许多个接收者 clone 共享同一个 future
```

**`GracefulShutdown` + `GracefulShutdownGuard`（`shutdown.rs:17`）**

```rust
pub struct GracefulShutdown {
    shutdown: Shutdown,
    guard: Option<GracefulShutdownGuard>,
}

// 当 shutdown 触发时，Future 返回 guard
impl Future for GracefulShutdown {
    type Output = GracefulShutdownGuard;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(self.shutdown.poll_unpin(cx));
        Poll::Ready(self.guard.take().unwrap())
    }
}

pub struct GracefulShutdownGuard(Arc<AtomicUsize>);

impl GracefulShutdownGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);  // 创建时 +1
        Self(counter)
    }
}

impl Drop for GracefulShutdownGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);  // drop 时 -1
    }
}
```

### TaskManager 的关机等待循环（`lib.rs:288`）

```rust
fn do_graceful_shutdown(self, timeout: Option<Duration>) -> bool {
    drop(self.signal);  // 触发关机信号（signal 关闭 → Shutdown 所有 clone 都会 resolve）
    let when = timeout.map(|t| Instant::now() + t);
    while self.graceful_tasks.load(Ordering::Relaxed) > 0 {
        if when.map(|w| Instant::now() > w).unwrap_or(false) {
            debug!("graceful shutdown timed out");
            return false;
        }
        thread::yield_now();  // 主动让出 CPU，等待 graceful task 释放 guard
    }
    debug!("gracefully shut down");
    true
}
```

这是关机路径上的短暂等待循环。当前实现使用 `thread::yield_now()`，并支持超时版本防止无限等待。

---

## 6. 两类阻塞工作：tokio blocking pool vs rayon pool

这是 reth 并发设计中最重要的分工原则：

### tokio blocking pool（`spawn_blocking`）

适合：**有大量等待的阻塞 I/O**，如磁盘读、DB 查询。

```
tokio async 任务
  │  需要读数据库（阻塞调用）
  ▼
tokio::task::spawn_blocking(|| {
    // 在 blocking pool 线程上运行
    db.get(key)  // 这里可以阻塞，不影响 async 运行时
})
.await
```

**为什么不直接在 async 任务里调用阻塞 I/O？**

tokio 的 async 线程不能被阻塞——如果一个 async 任务卡住，整个线程上的其他任务都无法推进。`spawn_blocking` 把阻塞工作移到 tokio 专门的阻塞线程池（默认最多 512 个线程）。

**tokio blocking pool 的缺点**：CPU 密集型任务如果长时间不让出，会把 blocking pool 占满，其他 blocking 调用（包括 DB 读）就得排队。

### rayon thread pools（`BlockingTaskPool` / named worker pools）

适合：**纯 CPU 密集计算**，如状态根哈希、密码学运算、debug_traceTransaction。

```
async 任务
  │  需要进行大量 CPU 计算（keccak、Merkle）
  ▼
runtime.rpc_pool().spawn(move || {
    // 在 rayon 线程池运行（专门的 CPU 密集线程池）
    compute_state_root(...)
})
.await  // BlockingTaskHandle 实现了 Future
```

**为什么 CPU 密集任务不能用 tokio blocking pool？**

引用 reth 代码注释（[pool.rs:47](../../crates/tasks/src/pool.rs#L47)）：

> RPC calls that perform blocking IO (disk lookups) are not executed on this pool but on the tokio runtime's blocking pool, which performs **poorly** with CPU bound tasks. Once the tokio blocking pool is **saturated** it is converted into a queue, blocking tasks could then **interfere** with the queue and block other RPC calls.

### 分工总结

```
任务类型                    线程池              方法
─────────────────────────────────────────────────────────
异步等待（网络、timer）     tokio worker         直接 async/await
阻塞 I/O（DB 读/写）        tokio blocking pool  spawn_blocking
CPU 密集（trace、Merkle）   rayon pool           BlockingTaskPool::spawn
两者混合（eth_call）        tokio blocking pool  spawn_blocking_io（RPC 层）
```

---

## 7. BlockingTaskPool 与 BlockingTaskGuard

### BlockingTaskPool（`crates/tasks/src/pool.rs:55`）

```rust
pub struct BlockingTaskPool {
    pool: Arc<rayon::ThreadPool>,
}
```

核心方法：

```rust
pub fn spawn<F, R>(&self, func: F) -> BlockingTaskHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = oneshot::channel();  // tokio oneshot

    self.pool.spawn(move || {
        // 在 rayon 线程上运行
        let _result = tx.send(catch_unwind(AssertUnwindSafe(func)));
        // panic 被 catch_unwind 捕获，通过 oneshot 发回
    });

    BlockingTaskHandle { rx }
}
```

`BlockingTaskHandle<R>` 实现了 `Future<Output = thread::Result<R>>`，通过 tokio oneshot channel 桥接 rayon（同步）→ tokio（异步）的结果传递。

**panic 处理**：rayon 任务内的 panic 被 `catch_unwind(AssertUnwindSafe(func))` 捕获，以 `Err(Box<dyn Any>)` 的形式通过 oneshot 发回，不会 crash 整个进程。

### BlockingTaskGuard（`pool.rs:22`）

```rust
pub struct BlockingTaskGuard(Arc<Semaphore>);

impl BlockingTaskGuard {
    pub fn new(max_blocking_tasks: usize) -> Self {
        Self(Arc::new(Semaphore::new(max_blocking_tasks)))
    }

    pub async fn acquire_owned(self) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.0.acquire_owned().await
    }

    pub async fn acquire_many_owned(self, n: u32) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.0.acquire_many_owned(n).await
    }
}
```

`BlockingTaskGuard` 是一个信号量包装器，用于限制并发任务数量。在 RPC 层，`debug_traceTransaction` 等 CPU 密集调用会先 `acquire_owned().await`，拿到 permit 后才开始工作，确保不超过系统容量。`Runtime` 还维护了多个 rayon 池：通用 `cpu_pool`、RPC `rpc_pool`、存储写入 `storage_pool`，以及 proof、prewarming、BAL、state trie overlay 等 named worker pools。

---

## 8. Channel 选型：何时用哪种 channel？

reth 使用多种 channel，每种有不同的使用场景：

### 决策树

```
需要 channel？
│
├── 只发一次结果（RPC 调用返回值、异步任务结果）
│     └── tokio::sync::oneshot::channel()
│           （BlockingTaskPool 内部、spawn_blocking_io 返回值）
│
├── 多个生产者 → 单个消费者
│     ├── 需要背压？（消费者跟不上时要让生产者等待）
│     │     └── tokio::sync::mpsc::channel(N)（有界 channel）
│     │           （EthRequestHandler：有界防 DoS）
│     │
│     └── 不需要背压（允许堆积）
│           └── tokio::sync::mpsc::unbounded_channel()
│                 （NetworkHandle → NetworkManager 的命令 channel）
│                 （TaskManager 的 task_events channel）
│
└── 同步上下文（非 async 代码）
      └── std::sync::mpsc::sync_channel(1)
            （并行状态根：rayon 线程 → 主线程传递结果）
```

### 各 channel 的实际用例

**`tokio::sync::oneshot`**：
- `BlockingTaskPool::spawn()` 内部用于把 rayon 结果传回 async 上下文
- RPC `spawn_blocking_io` 内部

**`tokio::sync::mpsc::unbounded_channel`**：
- `TaskManager` 接收任务事件（panic、graceful shutdown 请求）
- `NetworkHandle` 向 `NetworkManager` 发送命令（unbounded：因为命令量小，不需要背压）

**`tokio::sync::mpsc::channel(N)`**（有界）：
- `EthRequestHandler` 接收区块请求（有界 = 防止网络洪水攻击）
- 交易池事件广播（容量 1024）

**`std::sync::mpsc::sync_channel(1)`**：
- 并行状态根中，每个 storage root 计算任务用一个容量为 1 的同步 channel 传回结果
- 容量 1：发送方（rayon 线程）在接收方（主线程）未 recv 前会阻塞，天然背压

### 为什么并行状态根用 `std::sync::mpsc` 而非 tokio channel？

关键原因：`ParallelStateRoot::calculate()` 是**同步函数**（返回 `Result` 而非 `Future`），在 tokio blocking pool 线程上运行。同步代码里无法 `.await`，所以不能使用 `tokio::sync::oneshot`，只能用 `std::sync::mpsc`。

---

## 9. 锁策略：parking_lot vs tokio sync

### parking_lot::RwLock（同步热路径）

reth 在所有**同步代码的热路径**上使用 `parking_lot::RwLock`，而非标准库的 `std::sync::RwLock`：

```rust
// crates/transaction-pool/src/pool/mod.rs
pub(crate) struct PoolInner<V, T, S> {
    identifiers: RwLock<SenderIdentifiers>,   // 发送者 ID 映射
    pool: RwLock<TxPool<T>>,                  // 核心交易池
    event_listener: RwLock<PoolEventBroadcast<T>>,  // 事件监听器
    pending_transaction_listener: RwLock<Vec<...>>, // pending 交易监听
    transaction_listener: RwLock<Vec<...>>,         // 全部交易监听
    blob_transaction_sidecar_listener: Mutex<Vec<...>>, // blob sidecar 监听
    ...
}
```

**为什么选 parking_lot？**

- `parking_lot::RwLock` 比 `std::sync::RwLock` 快约 2-5 倍（内部使用更高效的 futex 实现）
- 支持公平性模式，避免写锁饥饿
- API 更简洁（无需 `.unwrap()` 处理毒化锁）

**锁持有时间原则**：只在需要保护共享状态时持有锁，**不在锁内做 I/O 或复杂计算**。交易池的典型模式是：拿锁 → 修改内存状态 → 立即释放 → 锁外做其他工作。

### tokio::sync::Semaphore（异步限流）

对于 **async 上下文中的资源限制**，使用 tokio 的 Semaphore：

```rust
// RPC 层 BlockingTaskGuard
let permit = guard.acquire_owned().await?;  // 异步获取 permit
// permit drop 时自动释放
blocking_pool.spawn(move || {
    let _permit = permit;  // 持有到任务完成
    do_heavy_work()
}).await
```

tokio `Semaphore` 的 `acquire_owned().await` 是异步的，等待期间**不阻塞线程**，可以同时处理其他请求。

---

## 10. tokio::select! 的使用模式

`tokio::select!` 在 reth 中有三类固定用法：

### 模式一：主任务 vs 关键任务崩溃监测（CLI runner）

```rust
// crates/cli/runner/src/lib.rs:243
tokio::select! {
    task_manager_result = tasks => {
        // TaskManager future 完成 = 有 critical task panic 了
        if let Err(panicked_error) = task_manager_result {
            return Err(panicked_error.into());
        }
    },
    res = fut => res?,  // 主命令正常完成
}
```

这个 `select!` 是 reth 程序的"顶层守卫"——同时监视两件事：主程序是否正常完成，以及是否有关键任务崩溃。

### 模式二：信号监听（优雅退出）

```rust
// crates/cli/runner/src/lib.rs:273
tokio::select! {
    _ = ctrl_c => { trace!("Received ctrl-c"); },
    _ = sigterm => { trace!("Received SIGTERM"); },  // Unix only
    res = fut => res?,
}
```

监听系统信号（Ctrl-C / SIGTERM）和主任务，任一完成即退出。

### 模式三：Actor 内部消息循环

在各类 Actor（NetworkManager、TransactionsManager 等）的 `poll()` 实现里，`select!` 用于同时监听多个事件源，实现无锁并发：

```rust
// NetworkManager 概念性示意
loop {
    tokio::select! {
        msg = self.from_handle_rx.recv() => {
            // 处理来自 NetworkHandle 的命令
            self.on_handle_message(msg);
        }
        event = self.swarm.next() => {
            // 处理 P2P 网络事件
            self.on_swarm_event(event);
        }
        _ = &mut shutdown => {
            // 收到关机信号，退出
            break;
        }
    }
}
```

**为什么用 `select!` 而不是多线程？**

Actor 模式配合 `select!` 实现了**单线程并发**：没有锁争用，没有数据竞争，代码推理简单，性能极好。每个 Actor 是一个独立的 Future，tokio 调度器负责在正确的时机 wake 它。

---

## 11. 并行状态根：深度案例

并行状态根计算（[crates/trie/parallel/src/root.rs](../../crates/trie/parallel/src/root.rs)）是 reth 里最能体现多种并发技术综合运用的场景。

### 为什么需要并行？

状态根计算需要：
1. **遍历账户 trie**（主线程，顺序）
2. **为每个被修改的账户计算 storage root**（各账户独立，可并行）

storage root 计算是 DB I/O 密集的（需要读该账户的存储 trie），可以并行化。

### 核心结构（`root.rs:39`）

```rust
pub struct ParallelStateRoot<Factory> {
    factory: Factory,          // DB provider 工厂（可 clone，每个 worker 独立创建连接）
    prefix_sets: TriePrefixSets, // 标记哪些账户/存储被修改
}
```

### 计算流程（`calculate` 方法，`root.rs:82`）

```rust
fn calculate(self, retain_updates: bool) -> Result<(B256, TrieUpdates), ...> {
    // 1. 确定哪些账户的 storage root 需要重新计算
    let storage_root_targets = StorageRootTargets::new(...);

    // 2. 获取 tokio runtime handle（供 spawn_blocking 使用）
    let handle = get_tokio_runtime_handle();

    // 3. 并行启动所有 storage root 计算任务
    let mut storage_roots = HashMap::with_capacity(storage_root_targets.len());
    for (hashed_address, prefix_set) in storage_root_targets {
        let factory = self.factory.clone(); // 每个 worker 克隆自己的 factory
        let (tx, rx) = mpsc::sync_channel(1); // std::sync::mpsc，容量1

        // 在 tokio blocking pool 上运行（I/O 密集，不用 rayon）
        drop(handle.spawn_blocking(move || {
            let result = (|| -> Result<_, _> {
                let provider = factory.database_provider_ro()?;
                Ok(StorageRoot::new_hashed(&provider, &provider, hashed_address, prefix_set)
                    .calculate(retain_updates)?)
            })();
            let _ = tx.send(result); // 发送结果（capacity=1，此处可能阻塞直到主线程 recv）
        }));

        storage_roots.insert(hashed_address, rx); // 用 address 索引 receiver
    }

    // 4. 主线程顺序遍历账户 trie
    let mut account_node_iter = TrieNodeIter::state_trie(...);
    while let Some(node) = account_node_iter.try_next()? {
        match node {
            TrieElement::Branch(node) => {
                hash_builder.add_branch(node.key, node.value, node.children_are_in_trie);
            }
            TrieElement::Leaf(hashed_address, account) => {
                // 5. 遇到叶子节点时，等待对应 storage root 任务完成
                let storage_root_result = match storage_roots.remove(&hashed_address) {
                    Some(rx) => rx.recv()??,  // 阻塞等待 worker 完成
                    None => {
                        // "missed leaf"：不在修改集中的账户，同步计算
                        tracker.inc_missed_leaves();
                        StorageRoot::new_hashed(...).calculate(retain_updates)?
                    }
                };
                // 6. 用 storage_root 更新 hash_builder
                // ...
            }
        }
    }

    Ok((hash_builder.root(), trie_updates))
}
```

### 关键设计细节

**`drop(handle.spawn_blocking(...))`**：`spawn_blocking` 返回 `JoinHandle`，显式 `drop` 而不是 `.await`，因为我们不用 `JoinHandle` 来等待——我们用 `mpsc::sync_channel` 的 `rx.recv()` 来同步。`JoinHandle` 被 drop 后任务仍然运行（detached）。

**`sync_channel(1)` 而非 `channel()`**：
- 容量为 1，意味着 sender（worker）发送后无法再发，天然防止 worker 泄漏多个结果
- worker 完成后立即 send，然后退出；主线程在需要时 recv

**`get_tokio_runtime_handle()`（`root.rs:276`）**：

```rust
pub fn get_tokio_runtime_handle() -> Handle {
    Handle::try_current().unwrap_or_else(|_| {
        // 如果在非 tokio 上下文中调用（例如测试），
        // 创建一个静态全局 runtime（OnceLock 保证只创建一次）
        static RT: OnceLock<Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            Builder::new_multi_thread()
                .enable_all()
                .thread_keep_alive(Duration::from_secs(15))
                .thread_name("trie-tokio-rt")
                .build()
                .expect("Failed to create tokio runtime")
        });
        rt.handle().clone()
    })
}
```

这个函数确保无论在 tokio runtime 内部还是外部调用，都能拿到可用的 Handle。静态 `OnceLock<Runtime>` 保证备用 runtime 只创建一次。

---

## 12. 一次完整的并行状态根计算流程

以处理一个包含 1000 个账户修改的区块为例：

```
Engine API 接收新 payload
    │
    ▼
执行所有交易（顺序，单线程）
    │  → 产生 "被修改的账户集合" (prefix_sets)
    ▼
计算状态根（需要在 payload 插入前完成）
    │
    ▼ ParallelStateRoot::calculate()
    │
    ├── 主线程：确定 1000 个账户的 storage root targets
    │
    ├── 并行启动 1000 个 tokio::spawn_blocking 任务
    │     每个任务：
    │     1. 从 factory clone 一个 DB provider
    │     2. 读取该账户的存储 trie（DB I/O）
    │     3. 计算 storage root（keccak）
    │     4. 发送到 sync_channel(1)
    │
    └── 主线程顺序遍历账户 trie（从数据库读 trie 节点）
          每遇到一个叶子节点：
          └── rx.recv() 等待对应 storage root 完成
              │  （此时很可能已经完成，几乎无等待）
              └── 把 storage_root 编码进 account_rlp → hash_builder
    │
    ▼
返回 state_root（B256）
    │
    ▼
插入区块、更新 canonical head
```

**为什么并行化有效？**

遍历账户 trie（顺序）和计算每个账户的 storage root（独立）是**两条可并行的工作流**。在主线程遍历 trie 时，各 storage root 的 I/O 在后台并发进行。当主线程到达某个叶子节点时，该账户的 storage root 计算往往已经完成（`rx.recv()` 立即返回），实现了接近 O(1) 的等待时间。

---

## 13. 关键设计决策

### 决策一：为什么不用 Rayon 做 storage root 并行化？

实际代码用的是 `tokio::spawn_blocking` 而非 rayon。原因：

- storage root 计算的瓶颈是**数据库 I/O**（读取 MDBX 存储 trie），不是 CPU
- tokio blocking pool 专门为阻塞 I/O 设计，有操作系统级别的线程调度优化
- rayon 的工作窃取算法适合 CPU 密集的计算图，对 I/O 等待效率较低

### 决策二：为什么 `TaskManager` 是一个 Future 而不是一个后台线程？

如果 `TaskManager` 是独立线程，检测到 panic 后无法在 async 上下文中优雅地向主程序传递错误。作为 `Future`，它可以直接参与 `tokio::select!`，让 panic 处理成为主程序异步流的一部分。

### 决策三：为什么 Shutdown 使用 `Shared<oneshot::Receiver<()>>` 而非 `broadcast`？

- `broadcast` 有容量限制，如果消费者太多可能丢失消息
- `Shared<oneshot::Receiver<()>>`：任意数量的 clone 共享同一个底层 future，一旦触发所有 clone 都会 resolve，零丢失

`futures_util::future::Shared` 是关键——它允许一个 `Future` 被多个 Waker 监听。触发一次 → 所有等待者全部唤醒，类似于多播机制。

### 决策四：GracefulShutdown 的等待循环是 bug 还是设计？

`do_graceful_shutdown` 里的等待循环（`while counter > 0 { thread::yield_now() }`）看起来简单，但有其合理性：

- 关机是**罕见操作**（程序生命周期只发生一次）
- 等待时间预期**极短**（几毫秒到几十毫秒）
- `yield_now` 会主动让出当前线程，避免纯自旋占满 CPU
- 配合超时（`graceful_shutdown_with_timeout`）防止无限等待

### 决策五：parking_lot vs std::sync vs tokio::sync 的选型原则

```
同步上下文，锁持有时间短          → parking_lot::RwLock / Mutex
同步上下文，需要读多写少优化      → parking_lot::RwLock
异步上下文，需要 .await 等待      → tokio::sync::Mutex / RwLock
异步上下文，信号量限流            → tokio::sync::Semaphore
简单一次性初始化                  → std::sync::OnceLock
```

**重要规则**：永远不要在 async 任务中持有 `parking_lot` 锁跨越 `.await` 点——这会导致锁在 await 挂起期间持续阻塞其他需要锁的同步代码。

---

## 本章小结

```
reth 并发体系一览：

【顶层】
  tokio multi-thread runtime（工作线程 = CPU 核心数，keep_alive=15s）
      │
      ├── Runtime / TaskExecutor（可 clone，spawn 各类任务）
      │       └── TaskManager（Future，监控 critical task panic）
      │
      ├── 各 Actor（NetworkManager、EngineTree、PayloadBuilderService...）
      │   每个是一个 Future，单线程无锁，通过 channel 通信
      │
      └── tokio::select! 驱动事件循环

【阻塞层】
  tokio blocking pool
      └── I/O 密集任务（DB 读取、并行 storage root 计算）
              使用 std::sync::mpsc::sync_channel(1) 传回结果

  rayon thread pool（BlockingTaskPool）
      └── CPU 密集任务（debug_trace、eth_getProof）
              使用 tokio::sync::oneshot 传回结果

【限流层】
  tokio::sync::Semaphore（BlockingTaskGuard）
      └── RPC 并发限制（IO pool / Tracing pool 各自的 permit 数）
```

---

*下一章（附录）：从 `reth node` 命令到全节点就绪的启动流程。*
