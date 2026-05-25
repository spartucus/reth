//! 演示三种任务调度模式的区别

use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    println!("=== 三种任务调度模式对比 ===\n");

    let start = Instant::now();

    // 模式 1: tokio::spawn - 标准异步任务
    println!("【模式 1】tokio::spawn(async {{}})");
    tokio::spawn(async {
        println!(
            "  [{:>5.2}s] 运行在: {:?} (Tokio 主线程池)",
            Instant::now().elapsed().as_secs_f32(),
            std::thread::current().name()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        println!("  [{:>5.2}s] 任务完成，线程可以处理其他任务", start.elapsed().as_secs_f32());
    })
    .await
    .unwrap();

    println!();

    // 模式 2: tokio::task::spawn_blocking - 同步代码
    println!("【模式 2】tokio::task::spawn_blocking(|| {{}}) - 只能同步代码");
    tokio::task::spawn_blocking(|| {
        println!(
            "  [{:>5.2}s] 运行在: {:?} (blocking 线程池)",
            start.elapsed().as_secs_f32(),
            std::thread::current().name()
        );
        // ❌ 这里不能 .await
        std::thread::sleep(Duration::from_millis(100));
        println!("  [{:>5.2}s] 任务完成，线程阻塞了 100ms", start.elapsed().as_secs_f32());
    })
    .await
    .unwrap();

    println!();

    // 模式 3: Reth 的混合模式 - async 代码在 blocking 线程运行
    println!("【模式 3】spawn_blocking + block_on - Reth 混合模式");
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        println!(
            "  [{:>5.2}s] 运行在: {:?} (blocking 线程池)",
            start.elapsed().as_secs_f32(),
            std::thread::current().name()
        );
        // ✅ 通过 block_on 可以运行 async 代码
        handle.block_on(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            println!("  [{:>5.2}s] async 代码在 blocking 线程中同步执行", start.elapsed().as_secs_f32());
        });
    })
    .await
    .unwrap();

    println!("\n=== 总结 ===");
    println!("✅ 模式 1: async 任务，非阻塞，线程可复用");
    println!("✅ 模式 2: 同步任务，阻塞，但不能 await");
    println!("✅ 模式 3: async 任务在 blocking 线程中同步执行");
}
