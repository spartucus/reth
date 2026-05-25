# Reth 节点启动流程

> 本文档记录 reth 节点从 `cargo run --bin reth node` 到完全启动的流程。所有代码引用均从源码直接验证。

---

## 启动入口

### main() 函数

**文件：** `bin/reth/src/main.rs:17`

```rust
// 源码：bin/reth/src/main.rs:17-49
fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) =
        Cli::<EthereumChainSpecParser, RessArgs>::parse().run(async move |builder, ress_args| {
            info!(target: "reth::cli", "Launching node");
            let NodeHandle { node, node_exit_future } =
                builder.node(EthereumNode::default()).launch_with_debug_capabilities().await?;

            // Install ress subprotocol.
            if ress_args.enabled {
                install_ress_subprotocol(
                    ress_args,
                    node.provider,
                    node.evm_config,
                    node.network,
                    node.task_executor,
                    node.add_ons_handle.engine_events.new_listener(),
                )?;
            }

            node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
```

### Cli 解析与路由

`Cli` 结构体定义在 `crates/ethereum/cli/src/interface.rs:36-57`，使用 clap 派生宏解析命令行参数。

```rust
// 源码：crates/ethereum/cli/src/interface.rs:36-57
pub struct Cli<
    C: ChainSpecParser = EthereumChainSpecParser,
    Ext: clap::Args + fmt::Debug = NoArgs,
    Rpc: RpcModuleValidator = DefaultRpcModuleValidator,
    SubCmd: Subcommand + fmt::Debug = NoSubCmd,
> {
    #[command(subcommand)]
    pub command: Commands<C, Ext, SubCmd>,
    #[command(flatten)]
    pub logs: LogArgs,
    #[command(flatten)]
    pub traces: TraceArgs,
    #[arg(skip)]
    pub _phantom: PhantomData<Rpc>,
}
```

`Cli::run()` 方法（`interface.rs:132-139`）创建 `CliRunner`，然后通过 `CliApp::run()` 路由到对应的子命令。

当用户执行 `reth node` 时，路由到 `Commands::Node`，调用 `NodeCommand::execute()`。

### NodeCommand 执行

**文件：** `crates/cli/commands/src/node.rs:157-226`

`NodeCommand::execute()` 的核心逻辑：

```rust
// 源码：crates/cli/commands/src/node.rs:214-225
tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
let database = init_db(db_path.clone(), self.db.database_args())?.with_metrics();

if with_unused_ports {
    node_config = node_config.with_unused_ports();
}

let builder = NodeBuilder::new(node_config)
    .with_database(database)
    .with_launch_context(ctx.task_executor);

launcher.entrypoint(builder, ext).await
```

在 `main()` 中传入的闭包调用 `builder.node(EthereumNode::default()).launch_with_debug_capabilities().await`，其中：

- `builder.node()` 方法（`crates/node/builder/src/builder/mod.rs:375-385`）调用 `node.components_builder()` 和 `node.add_ons()` 配置节点类型和组件
- `launch_with_debug_capabilities()` 方法（`mod.rs:698-715`）创建 `DebugNodeLauncher` 包裹 `EngineNodeLauncher`，然后调用 `launch_node()`

---

## 数据库初始化

### init_db

**文件：** `crates/storage/db/src/mdbx.rs:38-40`

```rust
// 源码：crates/storage/db/src/mdbx.rs:38-54
pub fn init_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    init_db_for::<P, Tables>(path, args)
}

pub fn init_db_for<P: AsRef<Path>, TS: TableSet>(
    path: P,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let client_version = args.client_version().clone();
    let mut db = create_db(path, args)?;
    db.create_and_track_tables_for::<TS>()?;
    db.record_client_version(client_version)?;
    drop_orphan_tables(&db);
    Ok(db)
}
```

### DatabaseEnv 结构

**文件：** `crates/storage/db/src/implementation/mdbx/mod.rs:244-257`

```rust
// 源码：crates/storage/db/src/implementation/mdbx/mod.rs:244-257
pub struct DatabaseEnv {
    /// Libmdbx-sys environment.
    inner: Environment,
    /// Opened DBIs for reuse.
    /// Important: Do not manually close these DBIs, like via `mdbx_dbi_close`.
    /// More generally, do not dynamically create, re-open, or drop tables at
    /// runtime. It's better to perform table creation and migration only once
    /// at startup.
    dbis: Arc<HashMap<&'static str, ffi::MDBX_dbi>>,
    /// Cache for metric handles. If `None`, metrics are not recorded.
    metrics: Option<Arc<DatabaseEnvMetrics>>,
    /// Write lock for when dealing with a read-write environment.
    _lock_file: Option<StorageLock>,
}
```

---

## 核心启动步骤

启动的核心逻辑在 `EngineNodeLauncher::launch_node()` 中。

**文件：** `crates/node/builder/src/launch/engine.rs:66-409`

### 第一阶段：基础设施（engine.rs:90-125）

启动上下文通过链式调用（type-state 模式）逐步构建：

```rust
// 源码：crates/node/builder/src/launch/engine.rs:94-125
let ctx = ctx
    .with_configured_globals(engine_tree_config.reserved_cpu_cores())
    // load the toml config
    .with_loaded_toml_config(config)?
    // add resolved peers
    .with_resolved_peers()?
    // attach the database
    .attach(database.clone())
    // ensure certain settings take effect
    .with_adjusted_configs()
    // Create the provider factory with changeset cache
    .with_provider_factory::<_, <CB::Components as NodeComponents<T>>::Evm>(changeset_cache.clone()).await?
    .inspect(|_| {
        info!(target: "reth::cli", "Database opened");
    })
    .with_prometheus_server().await?
    .inspect(|this| {
        debug!(target: "reth::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
    })
    .with_genesis()?
    .inspect(|this: &LaunchContextWith<Attached<WithConfigs<<T::Types as NodeTypes>::ChainSpec>, _>>| {
        info!(target: "reth::cli", "\n{}", this.chain_spec().display_hardforks());
        let settings = this.provider_factory().cached_storage_settings();
        info!(target: "reth::cli", ?settings, "Loaded storage settings");
    })
    .with_metrics_task()
    // passing FullNodeTypes as type parameter here so that we can build
    // later the components.
    .with_blockchain_db::<T, _>(move |provider_factory| {
        Ok(BlockchainProvider::new(provider_factory)?)
    })?
    .with_components(components_builder, on_component_initialized).await?;
```

各步骤含义：

1. **`with_configured_globals`** (`common.rs:210-244`)：提升文件描述符限制，配置 Rayon 全局线程池
2. **`with_loaded_toml_config`** (`common.rs:139-148`)：加载 `reth.toml` 配置文件
3. **`with_resolved_peers`** (`common.rs:305-316`)：将 `--trusted-peers` 加入 TOML 配置
4. **`attach(database)`**：将 `DatabaseEnv` 附加到上下文
5. **`with_adjusted_configs`** (`common.rs:346-348`)：调整 ETL 目录路径、端口号等
6. **`with_provider_factory`** (`common.rs:577-592`)：创建 `ProviderFactory`（含 StaticFileProvider、RocksDB、一致性检查）
7. **`with_prometheus_server`** (`common.rs:617-620`)：启动 Prometheus metrics HTTP 服务器
8. **`with_genesis`** (`common.rs:656-658`)：初始化 Genesis 块（如果数据库为空）
9. **`with_metrics_task`** (`common.rs:671-687`)：启动 stages metrics 监听任务
10. **`with_blockchain_db`** (`common.rs:710-734`)：创建 `BlockchainProvider`
11. **`with_components`** (`common.rs:776-829`)：初始化五大核心组件

### 组件初始化

**文件：** `crates/node/builder/src/components/builder.rs:375-403`

`ComponentsBuilder::build_components()` 按以下顺序初始化：

```rust
// 源码：crates/node/builder/src/components/builder.rs:388-402
let evm_config = executor_builder.build_evm(context).await?;
let pool = pool_builder.build_pool(context, evm_config.clone()).await?;
let network = network_builder.build_network(context, pool.clone()).await?;
let payload_builder_handle = payload_builder
    .spawn_payload_builder_service(context, pool.clone(), evm_config.clone())
    .await?;
let consensus = consensus_builder.build_consensus(context).await?;

Ok(Components {
    transaction_pool: pool,
    evm_config,
    network,
    payload_builder_handle,
    consensus,
})
```

对于 `EthereumNode`，具体组件构建器配置在 `crates/ethereum/node/src/node.rs:90-96`：

```rust
// 源码：crates/ethereum/node/src/node.rs:90-96
ComponentsBuilder::default()
    .node_types::<Node>()
    .pool(EthereumPoolBuilder::default())
    .executor(EthereumExecutorBuilder::default())
    .payload(BasicPayloadServiceBuilder::default())
    .network(EthereumNetworkBuilder::default())
    .consensus(EthereumConsensusBuilder::default())
```

### 第二阶段：Pipeline 准备（engine.rs:127-175）

```rust
// 源码：crates/node/builder/src/launch/engine.rs:128
let maybe_exex_manager_handle = ctx.launch_exex(installed_exex).await?;
```

启动 ExEx（Execution Extensions）管理器（如果有安装的扩展）。

```rust
// 源码：crates/node/builder/src/launch/engine.rs:148-161
let pipeline = build_networked_pipeline(
    &ctx.toml_config().stages,
    network_client.clone(),
    consensus.clone(),
    ctx.provider_factory().clone(),
    ctx.task_executor(),
    ctx.sync_metrics_tx(),
    ctx.prune_config(),
    max_block,
    static_file_producer,
    ctx.components().evm_config().clone(),
    maybe_exex_manager_handle.clone().unwrap_or_else(ExExManagerHandle::empty),
    ctx.era_import_source(),
)?;
```

`build_networked_pipeline`（`crates/node/builder/src/setup.rs:32-76`）构建 Staged Sync Pipeline，包含以下步骤：
- 创建 Header downloader 和 Body downloader
- 使用 `DefaultStages` 构建完整的 stage 集合（Headers, Bodies, SenderRecovery, Execution 等）
- 将 `ExecutionStage` 替换为带有 ExEx 支持的版本

```rust
// 源码：crates/node/builder/src/launch/engine.rs:164
pipeline.move_to_static_files()?;
```

确保 Static Files 是最新的。

```rust
// 源码：crates/node/builder/src/launch/engine.rs:168-175
let mut pruner_builder = ctx.pruner_builder();
if let Some(exex_manager_handle) = &maybe_exex_manager_handle {
    pruner_builder =
        pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
}
let pruner = pruner_builder.build_with_provider_factory(ctx.provider_factory().clone());
let pruner_events = pruner.events();
info!(target: "reth::cli", prune_config=?ctx.prune_config(), "Pruner initialized");
```

### 第三阶段：Engine 和 RPC（engine.rs:177-409）

**创建 EngineService：**

```rust
// 源码：crates/node/builder/src/launch/engine.rs:222-238
let mut engine_service = EngineService::new(
    consensus.clone(),
    ctx.chain_spec(),
    network_client.clone(),
    Box::pin(consensus_engine_stream),
    pipeline,
    Box::new(ctx.task_executor().clone()),
    ctx.provider_factory().clone(),
    ctx.blockchain_db().clone(),
    pruner,
    ctx.components().payload_builder_handle().clone(),
    engine_validator,
    engine_tree_config,
    ctx.sync_metrics_tx(),
    ctx.components().evm_config().clone(),
    changeset_cache,
);
```

**启动事件聚合任务：**

```rust
// 源码：crates/node/builder/src/launch/engine.rs:243-258
let events = stream_select!(
    event_sender.new_listener().map(Into::into),
    pipeline_events.map(Into::into),
    ctx.consensus_layer_events(),
    pruner_events.map(Into::into),
    static_file_producer_events.map(Into::into),
);

ctx.task_executor().spawn_critical(
    "events task",
    Box::pin(node::handle_events(
        Some(Box::new(ctx.components().network().clone())),
        Some(ctx.head().number),
        events,
    )),
);
```

**启动 RPC 服务（含 Auth Server）：**

```rust
// 源码：crates/node/builder/src/launch/engine.rs:260-266
let RpcHandle {
    rpc_server_handles,
    rpc_registry,
    engine_events,
    beacon_engine_handle,
    engine_shutdown: _,
} = add_ons.launch_add_ons(add_ons_ctx).await?;
```

RPC 启动逻辑在 `crates/node/builder/src/rpc.rs:887-898`（`launch_add_ons_with` 方法），同时启动 HTTP/WS JSON-RPC Server 和 Engine Auth Server（JWT 认证）。

**启动 Consensus Engine 主循环：**

```rust
// 源码：crates/node/builder/src/launch/engine.rs:374
ctx.task_executor().spawn_critical("consensus engine", Box::pin(consensus_engine));
```

Consensus engine 是一个 `tokio::select!` 循环（`engine.rs:304-370`），处理三类事件：
- `engine_service.next()`：处理 `ChainEvent`（BackfillSyncFinished, FatalError, Handler 等）
- `built_payloads`：接收本地构建的 payload，注入 engine tree
- `shutdown_rx`：处理引擎关闭请求

**返回 NodeHandle：**

```rust
// 源码：crates/node/builder/src/launch/engine.rs:400-408
let handle = NodeHandle {
    node_exit_future: NodeExitFuture::new(
        async { rx.await? },
        full_node.config.debug.terminate,
    ),
    node: full_node,
};

Ok(handle)
```

---

## 启动时序概览

```
main()
  |-- sigsegv handler + RUST_BACKTRACE
  |-- Cli::parse().run()
      |-- CliRunner 创建 tokio runtime
      |-- NodeCommand::execute()
          |-- init_db() 打开 MDBX 数据库
          |-- NodeBuilder::new().with_database().with_launch_context()
          |-- builder.node(EthereumNode::default())
              |-- 配置 ComponentsBuilder + AddOns
          |-- launch_with_debug_capabilities()
              |-- EngineNodeLauncher::launch_node()
                  |
                  |-- 第一阶段：基础设施
                  |   |-- 配置 Rayon 线程池 + fd limit
                  |   |-- 加载 reth.toml 配置
                  |   |-- 解析 trusted peers
                  |   |-- 创建 ProviderFactory（含一致性检查）
                  |   |-- 启动 Prometheus server
                  |   |-- 初始化 Genesis
                  |   |-- 启动 metrics 监听任务
                  |   |-- 创建 BlockchainProvider
                  |   |-- 初始化 5 大组件（EVM, Pool, Network, Payload, Consensus）
                  |
                  |-- 第二阶段：Pipeline 准备
                  |   |-- 启动 ExEx Manager（如果有）
                  |   |-- 构建 Staged Sync Pipeline
                  |   |-- move_to_static_files()
                  |   |-- 初始化 Pruner
                  |
                  |-- 第三阶段：服务启动
                  |   |-- 创建 EngineService
                  |   |-- 启动 events 聚合任务
                  |   |-- 启动 RPC 服务器（HTTP/WS + Engine Auth）
                  |   |-- 启动 Consensus Engine 主循环
                  |
                  |-- 返回 NodeHandle
```

---

## 关键代码位置

| 功能 | 文件 | 行号/函数 |
|------|------|-----------|
| main() 入口 | `bin/reth/src/main.rs` | 17 |
| Cli 定义 | `crates/ethereum/cli/src/interface.rs` | 36-57 |
| Cli::run() | `crates/ethereum/cli/src/interface.rs` | 132-139 |
| 命令路由 | `crates/ethereum/cli/src/app.rs` | 146-183 (`run_commands_with`) |
| NodeCommand::execute() | `crates/cli/commands/src/node.rs` | 157-226 |
| NodeBuilder | `crates/node/builder/src/builder/mod.rs` | 152-157 |
| NodeBuilder::node() | `crates/node/builder/src/builder/mod.rs` | 375-385 |
| launch_with_debug_capabilities() | `crates/node/builder/src/builder/mod.rs` | 698-715 |
| EngineNodeLauncher::launch_node() | `crates/node/builder/src/launch/engine.rs` | 66-409 |
| LaunchContext 定义 | `crates/node/builder/src/launch/common.rs` | 117-122 |
| configure_globals() | `crates/node/builder/src/launch/common.rs` | 221-244 |
| with_loaded_toml_config() | `crates/node/builder/src/launch/common.rs` | 139-148 |
| with_resolved_peers() | `crates/node/builder/src/launch/common.rs` | 305-316 |
| with_adjusted_configs() | `crates/node/builder/src/launch/common.rs` | 346-348 |
| create_provider_factory() | `crates/node/builder/src/launch/common.rs` | 458-574 |
| with_genesis() | `crates/node/builder/src/launch/common.rs` | 656-658 |
| with_metrics_task() | `crates/node/builder/src/launch/common.rs` | 671-687 |
| with_blockchain_db() | `crates/node/builder/src/launch/common.rs` | 710-734 |
| with_components() | `crates/node/builder/src/launch/common.rs` | 776-829 |
| ComponentsBuilder::build_components() | `crates/node/builder/src/components/builder.rs` | 375-403 |
| EthereumNode::components() | `crates/ethereum/node/src/node.rs` | 69-97 |
| init_db() | `crates/storage/db/src/mdbx.rs` | 38-40 |
| DatabaseEnv 定义 | `crates/storage/db/src/implementation/mdbx/mod.rs` | 244-257 |
| build_networked_pipeline() | `crates/node/builder/src/setup.rs` | 32-76 |
| build_pipeline() | `crates/node/builder/src/setup.rs` | 80-135 |
| launch_add_ons_with() | `crates/node/builder/src/rpc.rs` | 887-898 |
| EngineService 定义 | `crates/engine/service/src/service.rs` | 57-63 |
| LaunchNode trait | `crates/node/builder/src/launch/mod.rs` | 24-33 |
| NodeHandle 定义 | `crates/node/builder/src/handle.rs` | 10-15 |
| FullNode 定义 | `crates/node/builder/src/node.rs` | 107-126 |

---

## LaunchContext 类型演化

`LaunchContext` 使用 type-state 模式在编译期保证初始化顺序正确（`common.rs:108-115`）：

```text
LaunchContext
  -> LaunchContextWith<WithConfigs>
    -> LaunchContextWith<Attached<WithConfigs, DB>>
      -> LaunchContextWith<Attached<WithConfigs, ProviderFactory>>
        -> LaunchContextWith<Attached<WithConfigs, WithMeteredProviders>>
          -> LaunchContextWith<Attached<WithConfigs, WithComponents>>
```

每个方法只在对应的类型状态上可用，编译器会阻止错误的调用顺序。

---

## 关键设计决策

1. **组件初始化顺序**：EVM 先于 Pool（Pool 需要 EVM 配置来验证交易），Pool 先于 Network（Network 需要 Pool 来传播交易），Payload Builder 需要 Pool 和 EVM
2. **RPC 在 Engine 前启动**：CL 启动后立即通过 Engine API 发送消息，RPC Auth Server 必须先就绪
3. **Pipeline 启动时执行 `move_to_static_files()`**：确保冷数据已归档，避免启动后触发大量迁移
4. **使用 `spawn_critical`**：关键任务 panic 会导致节点退出，避免部分功能失效时节点"半死不活"
