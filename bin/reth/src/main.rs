#![allow(missing_docs)]

// We use jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "optimism", not(test)))]
compile_error!("Cannot build the `reth` binary with the `optimism` feature flag enabled. Did you mean to build `op-reth`?");

#[cfg(not(feature = "optimism"))]
fn main() {
    use reth::cli::Cli;
    use reth_node_ethereum::EthereumNode;

    reth::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) = Cli::parse_args().run(|builder, _| async {
        let compiler_config = &builder.config().experimental.compiler;
        #[cfg(not(feature = "compiler"))]
        {
            eyre::ensure!(
                !compiler_config.compiler,
                "`experimental.compiler` is set, but reth was not compiled with the `compiler` feature enabled"
            );
            let node = EthereumNode::default();
            let handle = builder.launch_node(node).await?;
            handle.node_exit_future.await
        }
        #[cfg(feature = "compiler")]
        {
            let executor_builder = reth::compiler::CompilerExecutorBuilder::default();
            let handle = builder
                .with_types::<EthereumNode>()
                .with_components(EthereumNode::components().executor(executor_builder))
                .launch()
                .await?;
            handle.node_exit_future.await
        }
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
