//! 演示 tokio 的两种线程池：
//! 1. 主运行时线程池（异步任务）
//! 2. spawn_blocking 线程池（阻塞任务）
//!
//! 以及 thread_keep_alive 的作用

use std::time::Duration;

fn main() {
    println!("=== Tokio 线程池架构 ===\n");

    // 场景 1: 短 keep_alive 时间（1秒）
    println!("【场景 1】thread_keep_alive = 1 秒");
    short_keep_alive_example();

    println!("\n---\n");

    // 场景 2: 长 keep_alive 时间（15秒）
    println!("【场景 2】thread_keep_alive = 15 秒（reth 的配置）");
    long_keep_alive_example();
}

/// 演示短 keep_alive 时间（1秒）的影响
fn short_keep_alive_example() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_keep_alive(Duration::from_secs(1))  // 短时间
        .worker_threads(2)
        .max_blocking_threads(4)
        .build()
        .unwrap();

    runtime.block_on(async {
        println!("启动时创建 blocking 线程池，初始大小 = 0");

        // 第一次阻塞任务
        println!("\n[时刻 0s] 执行第一个阻塞任务");
        tokio::task::spawn_blocking(|| {
            println!("  → 线程池创建新线程 (Thread-1)");
            std::thread::sleep(Duration::from_millis(100));
            println!("  → Thread-1 完成任务");
        })
        .await
        .unwrap();

        println!("[时刻 0.1s] Thread-1 进入空闲状态，等待复用");

        // 等待 2 秒（超过 keep_alive）
        println!("[时刻 0.1s ~ 2.1s] 空闲 2 秒...");
        tokio::time::sleep(Duration::from_secs(2)).await;

        println!("[时刻 2.1s] Thread-1 已超过 keep_alive(1s)，被销毁 ❌");

        // 第二次阻塞任务（需要重新创建线程）
        println!("\n[时刻 2.1s] 执行第二个阻塞任务");
        tokio::task::spawn_blocking(|| {
            println!("  → 线程池重新创建新线程 (Thread-2) 💰 开销！");
            std::thread::sleep(Duration::from_millis(100));
            println!("  → Thread-2 完成任务");
        })
        .await
        .unwrap();
    });

    println!("\n结论：频繁创建/销毁线程 → 性能开销大");
}

/// 演示长 keep_alive 时间（15秒）的影响
fn long_keep_alive_example() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_keep_alive(Duration::from_secs(15))  // 长时间（reth 配置）
        .worker_threads(2)
        .max_blocking_threads(4)
        .build()
        .unwrap();

    runtime.block_on(async {
        println!("启动时创建 blocking 线程池，初始大小 = 0");

        // 第一次阻塞任务
        println!("\n[时刻 0s] 执行第一个阻塞任务");
        tokio::task::spawn_blocking(|| {
            println!("  → 线程池创建新线程 (Thread-1)");
            std::thread::sleep(Duration::from_millis(100));
            println!("  → Thread-1 完成任务");
        })
        .await
        .unwrap();

        println!("[时刻 0.1s] Thread-1 进入空闲状态，等待复用");

        // 模拟区块间隔（12 秒）
        println!("[时刻 0.1s ~ 12.1s] 空闲 12 秒（模拟区块间隔）...");
        tokio::time::sleep(Duration::from_secs(12)).await;

        println!("[时刻 12.1s] Thread-1 仍在 keep_alive(15s) 内，保持存活 ✅");

        // 第二次阻塞任务（复用现有线程）
        println!("\n[时刻 12.1s] 执行第二个阻塞任务");
        tokio::task::spawn_blocking(|| {
            println!("  → 复用现有线程 (Thread-1) ⚡ 零开销！");
            std::thread::sleep(Duration::from_millis(100));
            println!("  → Thread-1 完成任务");
        })
        .await
        .unwrap();
    });

    println!("\n结论：线程复用 → 避免创建开销 → 性能提升");
}
