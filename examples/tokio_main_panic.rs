// 演示 async fn main() panic 会导致进程退出
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    println!("==============================================");
    println!("async fn main() panic 测试");
    println!("==============================================\n");

    println!("启动后台 spawn 任务...");

    // 启动一些长时间运行的后台任务
    let _task1 = tokio::spawn(async {
        for i in 1..=100 {
            sleep(Duration::from_millis(100)).await;
            println!("  后台任务1: 迭代 {}", i);
        }
        println!("  后台任务1: 完成（这行不会打印）");
    });

    let _task2 = tokio::spawn(async {
        for i in 1..=100 {
            sleep(Duration::from_millis(150)).await;
            println!("  后台任务2: 迭代 {}", i);
        }
        println!("  后台任务2: 完成（这行不会打印）");
    });

    println!("后台任务已启动\n");

    sleep(Duration::from_secs(1)).await;
    println!("倒计时: 3...");
    sleep(Duration::from_secs(1)).await;
    println!("倒计时: 2...");
    sleep(Duration::from_secs(1)).await;
    println!("倒计时: 1...");
    sleep(Duration::from_secs(1)).await;

    println!("\n💥 async main 即将 panic！");
    println!("观察: 进程会立即退出，后台 spawn 任务也会被终止\n");

    // async main 中 panic - 会导致进程退出
    panic!("async main panic - 进程将退出！");

    // 下面的代码永远不会执行
    #[allow(unreachable_code)]
    {
        println!("这行永远不会打印");
    }
}
