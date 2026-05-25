// 演示 tokio::spawn 任务 panic 不会导致进程退出
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    println!("=== 主程序开始 ===\n");

    // 任务1: 会 panic 的任务
    let handle1 = tokio::spawn(async {
        println!("任务1: 开始执行");
        sleep(Duration::from_millis(100)).await;
        println!("任务1: 准备 panic!");
        panic!("任务1 故意 panic!");
        #[allow(unreachable_code)]
        {
            println!("任务1: 这行永远不会执行");
        }
    });

    // 任务2: 正常任务
    let handle2 = tokio::spawn(async {
        for i in 1..=5 {
            sleep(Duration::from_millis(200)).await;
            println!("任务2: 正在运行 - 迭代 {}", i);
        }
        println!("任务2: 正常完成");
    });

    // 任务3: 另一个正常任务
    let handle3 = tokio::spawn(async {
        sleep(Duration::from_millis(150)).await;
        println!("任务3: 我在任务1 panic 之后仍然正常运行");
        sleep(Duration::from_millis(300)).await;
        println!("任务3: 正常完成");
    });

    println!("所有任务已启动\n");

    // 等待任务1（会 panic）
    match handle1.await {
        Ok(_) => println!("✓ 任务1 正常完成"),
        Err(e) => {
            if e.is_panic() {
                println!("✗ 任务1 发生了 panic: {:?}", e);
                // 尝试获取 panic 消息
                if let Ok(panic_msg) = e.try_into_panic() {
                    if let Some(s) = panic_msg.downcast_ref::<&str>() {
                        println!("  Panic 消息: {}", s);
                    } else if let Some(s) = panic_msg.downcast_ref::<String>() {
                        println!("  Panic 消息: {}", s);
                    }
                }
            } else {
                println!("✗ 任务1 被取消");
            }
        }
    }

    println!("\n任务1 panic 后，主程序继续运行...\n");

    // 等待其他任务
    handle2.await.unwrap();
    handle3.await.unwrap();

    println!("\n=== 所有任务完成，进程正常退出 ===");
}
