# Panic 行为对比：OS 线程 vs Tokio 任务

本文档对比了 OS 线程和 Tokio 异步任务在 panic 情况下的不同行为。

## 运行示例

```bash
# Tokio 任务 panic 测试
./run_example.sh tokio_panic_test

# OS 线程 panic 测试（多场景）
./run_example.sh thread_panic_test

# 主线程 panic 测试
./run_example.sh main_thread_panic_test
```

## 对比总结表

| 场景 | OS 线程 (std::thread) | Tokio 任务 (tokio::spawn) |
|------|----------------------|--------------------------|
| **子线程/任务 panic + join/await** | panic 被 join 捕获<br>返回 `Result::Err`<br>主线程继续运行 | panic 被 await 捕获<br>返回 `Result::Err`<br>主任务继续运行 |
| **子线程/任务 panic + 不 join/await** | 子线程崩溃退出<br>主线程**不知道**<br>进程继续运行 | 任务静默失败<br>主任务**不知道**<br>进程继续运行 |
| **主线程/主任务 panic** | **整个进程立即退出** ⚠️<br>所有子线程强制终止 | 运行时捕获 panic<br>进程**不会**退出 ✅ |
| **实现机制** | OS 调度<br>panic 通过 catch_unwind 捕获 | Tokio 调度器<br>每个任务自动包装 catch_unwind |
| **数量限制** | 受 OS 限制（通常几千个） | 可以百万级 |
| **上下文切换开销** | 昂贵（1-10μs） | 廉价（纳秒级） |
| **是否是真正的 OS 线程** | ✅ 是 | ❌ 否（运行在线程池上） |

## 详细场景分析

### 场景1：子线程/任务 panic + join/await 捕获

**OS 线程**：
```rust
let handle = thread::spawn(|| {
    panic!("子线程 panic!");
});

match handle.join() {
    Ok(_) => println!("正常结束"),
    Err(e) => println!("捕获到 panic: {:?}", e),
}
// ✅ 主线程继续运行
```

**Tokio 任务**：
```rust
let handle = tokio::spawn(async {
    panic!("任务 panic!");
});

match handle.await {
    Ok(_) => println!("正常结束"),
    Err(e) => println!("捕获到 panic: {:?}", e),
}
// ✅ 主任务继续运行
```

**结果**：两者行为相同，panic 被捕获，程序继续运行。

---

### 场景2：子线程/任务 panic + 不 join/await

**OS 线程**：
```rust
let _handle = thread::spawn(|| {
    panic!("子线程 panic!");
});
// 不 join，主线程继续
println!("主线程继续运行");
// ✅ 进程继续运行，但子线程崩溃了
```

**Tokio 任务**：
```rust
let _handle = tokio::spawn(async {
    panic!("任务 panic!");
});
// 不 await，主任务继续
println!("主任务继续运行");
// ✅ 进程继续运行，但任务静默失败
```

**结果**：两者行为相同，子线程/任务崩溃但不影响主程序。

---

### 场景3：主线程 panic ⚠️ **关键区别**

**OS 线程**：
```rust
fn main() {
    let _handle = thread::spawn(|| {
        loop {
            println!("后台线程运行中...");
            thread::sleep(Duration::from_secs(1));
        }
    });

    thread::sleep(Duration::from_secs(2));
    panic!("主线程 panic!");
    // ❌ 进程立即退出！
    // ❌ 所有子线程被强制终止！
}
```

**输出**：
```
后台线程运行中...
后台线程运行中...
thread 'main' panicked at 'main thread panic!'
// 进程退出，exit code 101
```

**Tokio 任务**：
```rust
#[tokio::main]
async fn main() {
    tokio::spawn(async {
        loop {
            println!("后台任务运行中...");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    tokio::time::sleep(Duration::from_secs(2)).await;
    panic!("主任务 panic!");
    // ✅ Tokio 运行时捕获 panic
    // ✅ 进程可能继续运行（取决于运行时配置）
}
```

**关键差异**：
- **OS 线程**：主线程 panic → **进程退出** ⚠️
- **Tokio 任务**：主任务 panic → **被运行时捕获** ✅

---

## Reth 中的应用

### spawn() - 普通任务

```rust
// crates/tasks/src/lib.rs
pub fn spawn<F>(&self, fut: F) -> JoinHandle<()> {
    // ❌ 没有显式 panic 捕获
    // ✅ 但 tokio::spawn 自动捕获了 panic
    // 任务 panic → 静默失败
    // TaskManager 不知道
    // 节点继续运行
    self.handle.spawn(fut)
}
```

**行为**：
- 任务 panic → tokio 捕获
- JoinHandle.await 返回 Err
- 但如果没有 await，panic 被忽略
- **节点继续运行**

### spawn_critical() - 关键任务

```rust
// crates/tasks/src/lib.rs
pub fn spawn_critical<F>(&self, name: &'static str, fut: F) -> JoinHandle<()> {
    let task = std::panic::AssertUnwindSafe(fut)
        .catch_unwind()
        .map_err(move |error| {
            // ✅ 显式捕获 panic
            let task_error = PanickedTaskError::new(name, error);
            error!("{task_error}");

            // ✅ 通知 TaskManager
            let _ = panicked_tasks_tx.send(TaskEvent::Panic(task_error));
        });

    self.handle.spawn(task)
}
```

**行为**：
- 任务 panic → 双重捕获（catch_unwind + tokio）
- 通过 channel 通知 TaskManager
- TaskManager 收到 `TaskEvent::Panic`
- TaskManager Future 返回 `Err`
- **节点主动退出** ⚠️

### TaskManager 如何响应

```rust
impl Future for TaskManager {
    type Output = Result<(), PanickedTaskError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.task_events_rx.poll_recv(cx) {
            // 收到 Panic 事件 → 返回错误
            Some(TaskEvent::Panic(err)) => Poll::Ready(Err(err)),
            _ => // ...
        }
    }
}
```

**节点启动代码**：
```rust
// crates/node/builder/src/launch/engine.rs
let task_manager = TaskManager::new(runtime.handle());
let executor = task_manager.executor();

// 启动关键任务
executor.spawn_critical("network", network_task);
executor.spawn_critical("engine", engine_task);

// 等待 TaskManager
match task_manager.await {
    Ok(()) => info!("节点正常关闭"),
    Err(panic_err) => {
        // 关键任务 panic
        error!("{panic_err}");
        std::process::exit(1); // 退出进程
    }
}
```

---

## 为什么 Tokio 任务 panic 不会导致进程退出？

### 核心原因

Tokio 任务不是 OS 线程，而是运行在线程池上的轻量级异步执行单元。

```
进程 Process
  └─ Tokio 运行时 Runtime
      ├─ OS 线程池 Thread Pool
      │   ├─ tokio-runtime-worker-1
      │   ├─ tokio-runtime-worker-2
      │   └─ tokio-runtime-worker-N
      │
      └─ 任务调度器 Scheduler
          ├─ Task 1 (可能 panic)
          ├─ Task 2 (正常运行)
          ├─ Task 3 (正常运行)
          └─ Task N...
```

### Tokio 内部实现（简化版）

```rust
// tokio 运行时内部逻辑
impl Runtime {
    fn spawn_task(&self, future: impl Future) -> JoinHandle {
        let task = async move {
            // ✅ 每个任务都被 catch_unwind 包装
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                block_on(future)
            }));

            match result {
                Ok(output) => TaskResult::Ok(output),
                Err(panic_payload) => {
                    // ✅ Panic 被捕获，不会传播
                    TaskResult::Panic(panic_payload)
                }
            }
        };

        self.scheduler.schedule(task)
    }
}
```

**关键点**：
1. 任务在 OS 线程上运行
2. 任务 panic 被 `catch_unwind` 捕获
3. **OS 线程本身没有 panic**
4. 线程继续运行其他任务
5. 进程不受影响

---

## 实战示例：观察 panic 行为

### 1. Tokio 任务 panic（进程不退出）

运行：
```bash
./run_example.sh tokio_panic_test
```

观察：
- 任务1 panic
- 任务2、任务3 继续运行
- 进程正常退出

### 2. OS 子线程 panic（进程不退出）

运行：
```bash
./run_example.sh thread_panic_test
```

观察：
- 场景1：join 捕获 panic
- 场景2：detached 线程 panic，主线程继续
- 进程正常退出

### 3. OS 主线程 panic（进程退出）⚠️

运行：
```bash
./run_example.sh main_thread_panic_test
```

观察：
- 后台线程正在运行（迭代计数增加）
- 主线程 panic
- **进程立即退出**（exit code 101）
- 后台线程被强制终止（没有打印"完成"）

---

## 总结

### OS 线程的 Panic 规则

1. ✅ **子线程 panic**：不影响进程（可通过 join 捕获）
2. ⚠️ **主线程 panic**：**整个进程退出**，所有子线程强制终止
3. 📊 **线程数量**：受 OS 限制（通常几千个）
4. 💰 **开销**：上下文切换昂贵

### Tokio 任务的 Panic 规则

1. ✅ **任务 panic**：被运行时捕获，不影响进程
2. ✅ **主任务 panic**：被运行时捕获（取决于如何处理）
3. 📊 **任务数量**：可以百万级
4. 💰 **开销**：上下文切换廉价（纳秒级）

### Reth 的设计选择

1. **spawn()**：
   - 任务 panic → 静默失败
   - TaskManager 不知道
   - 节点继续运行
   - 用于：非关键任务（监控、统计等）

2. **spawn_critical()**：
   - 任务 panic → 主动通知
   - TaskManager 收到事件
   - 节点主动退出
   - 用于：关键组件（网络、共识、引擎）

3. **设计理念**：**Fail-fast**
   - 关键组件 panic → 立即退出
   - 避免在损坏状态下继续运行
   - 通过进程重启恢复（systemd、k8s 等）

---

## 参考资料

- [Tokio 文档 - Runtime](https://docs.rs/tokio/latest/tokio/runtime/index.html)
- [Rust 文档 - std::panic::catch_unwind](https://doc.rust-lang.org/std/panic/fn.catch_unwind.html)
- [Reth Tasks 实现](../../crates/tasks/src/lib.rs)
