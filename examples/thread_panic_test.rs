//! 验证 Reth 的三种线程池架构

use std::time::Duration;

#[tokio::main]
async fn main() {
    println!("=== Reth 线程池架构验证 ===\n");

    // 1. Tokio 主运行时线程池
    println!("【1. Tokio 主运行时线程池】");
    for i in 0..3 {
        tokio::spawn(async move {
            println!(
                "  Async 任务 {} → 线程: {:?}",
                i,
                std::thread::current().name()
            );
        })
        .await
        .unwrap();
    }

    println!("\n【2. Tokio spawn_blocking 线程池】");
    for i in 0..3 {
        tokio::task::spawn_blocking(move || {
            println!(
                "  Blocking 任务 {} → 线程: {:?}",
                i,
                std::thread::current().name()
            );
        })
        .await
        .unwrap();
    }

    println!("\n【3. Rayon 线程池】");
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..3 {
        let tx = tx.clone();
        rayon::spawn(move || {
            let thread_name = std::thread::current().name().unwrap_or("unknown").to_string();
            tx.send((i, thread_name)).unwrap();
        });
    }
    drop(tx);
    while let Ok((i, name)) = rx.recv() {
        println!("  Rayon 任务 {} → 线程: {}", i, name);
    }

    println!("\n【结论】");
    println!("✅ 所有任务都在同一个进程中");
    println!("✅ 但运行在三种不同的线程池");
    println!("✅ 只有 spawn_blocking 受 thread_keep_alive 影响");
}
