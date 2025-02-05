mod backend;

use backend::{LlamacppNuma, LlamacppGGMLType, LlamacppSplitMode, LlamacppConfig, LlamacppBackend, BackendError};
use clap::{Parser};
use text_generation_router::{logging, server, usage_stats};
use thiserror::Error;
use tokenizers::{Tokenizer, FromPretrainedParameters};
use tokio::sync::oneshot::error::RecvError;
use tracing::{warn, error};

/// Backend Configuration
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Name of the model to load.
    #[clap(long, env)]
    model_id: String,

    /// Revision of the model.
    #[clap(default_value = "main", long, env)]
    revision: String,

    /// Path to the GGUF model file for inference.
    #[clap(long, env)]
    model_gguf: String, // TODO Option() with hf->gguf & quantize

    /// Number of threads to use for generation.
    #[clap(long, env)]
    n_threads: Option<usize>,

    /// Number of threads to use for batch processing.
    #[clap(long, env)]
    n_threads_batch: Option<usize>,

    /// Number of layers to store in VRAM.
    #[clap(default_value = "0", long, env)]
    n_gpu_layers: usize,

    /// Split the model across multiple GPUs.
    #[clap(default_value = "layer", long, env)]
    split_mode: LlamacppSplitMode,

    /// Defragment the KV cache if holes/size > threshold.
    #[clap(default_value = "-1.0", long, env)]
    defrag_threshold: f32,

    /// Enable NUMA optimizations.
    #[clap(default_value = "disabled", value_enum, long, env)]
    numa: LlamacppNuma,

    /// Use memory mapping for the model.
    #[clap(default_value = "true", long, env)]
    use_mmap: bool,

    /// Use memory locking to prevent swapping.
    #[clap(default_value = "false", long, env)]
    use_mlock: bool,

    /// Enable offloading of KQV operations to the GPU.
    #[clap(default_value = "false", long, env)]
    offload_kqv: bool,

    /// Enable flash attention for faster inference. (EXPERIMENTAL)
    #[clap(default_value = "true", long, env)]
    flash_attention: bool,

    /// Data type used for K cache.
    #[clap(default_value = "f16", value_enum, long, env)]
    type_k: LlamacppGGMLType,

    /// Data type used for V cache.
    #[clap(default_value = "f16", value_enum, long, env)]
    type_v: LlamacppGGMLType,

    /// Number of tokenizer workers used for payload validation and truncation.
    #[clap(default_value = "2", long, env)]
    validation_workers: usize,

    /// Maximum amount of concurrent requests.
    #[clap(long, env)]
    max_concurrent_requests: Option<usize>,

    /// Maximum number of input tokens per request.
    #[clap(default_value = "1024", long, env)]
    max_input_tokens: usize,

    /// Maximum total tokens (input + output) per request.
    #[clap(default_value = "2048", long, env)]
    max_total_tokens: usize,

    /// Maximum number of tokens in a batch.
    #[clap(long, env)]
    max_batch_total_tokens: Option<usize>,

    /// Maximum number of tokens in a physical batch.
    #[clap(long, env)]
    max_physical_batch_total_tokens: Option<usize>,

    /// Maximum number of requests per batch.
    #[clap(long, env)]
    max_batch_size: Option<usize>,

    /// IP address to listen on.
    #[clap(default_value = "0.0.0.0", long, env)]
    hostname: String,

    /// Port to listen on.
    #[clap(default_value = "3001", long, short, env)]
    port: u16,

    /// Enable JSON output format.
    #[clap(long, env)]
    json_output: bool,

    /// OTLP endpoint for telemetry data.
    #[clap(long, env)]
    otlp_endpoint: Option<String>,

    /// Service name for OTLP telemetry.
    #[clap(default_value = "text-generation-inference.router", long, env)]
    otlp_service_name: String,

    /// Allowed origins for CORS.
    #[clap(long, env)]
    cors_allow_origin: Option<Vec<String>>,

    /// Enable Ngrok tunneling.
    #[clap(long, env)]
    ngrok: bool,

    /// Ngrok authentication token.
    #[clap(long, env)]
    ngrok_authtoken: Option<String>,

    /// Ngrok edge to use for tunneling.
    #[clap(long, env)]
    ngrok_edge: Option<String>,

    /// Path to the tokenizer configuration file.
    #[clap(long, env)]
    tokenizer_config_path: Option<String>,

    /// Disable grammar support.
    #[clap(long, env, default_value_t = false)]
    disable_grammar_support: bool,

    /// Maximum number of inputs per request.
    #[clap(default_value = "4", long, env)]
    max_client_batch_size: usize,

    /// Level of usage statistics collection.
    #[clap(default_value = "on", long, env)]
    usage_stats: usage_stats::UsageStatsLevel,

    /// Maximum payload size limit in bytes.
    #[clap(default_value = "2000000", long, env)]
    payload_limit: usize,
}

#[tokio::main]
async fn main() -> Result<(), RouterError> {
    let args = Args::parse();

    logging::init_logging(
        args.otlp_endpoint,
        args.otlp_service_name,
        args.json_output
    );

    let n_threads = match args.n_threads {
        Some(0) | None => num_cpus::get(),
        Some(threads) => threads,
    };
    let n_threads_batch = match args.n_threads_batch {
        Some(0) | None => n_threads,
        Some(threads) => threads,
    };
    let max_batch_size = match args.max_batch_size {
        Some(0) | None => n_threads_batch,
        Some(threads) => threads,
    };
    let max_batch_total_tokens = match args.max_batch_total_tokens {
        None => max_batch_size * args.max_total_tokens,
        Some(size) => size,
    };
    let max_physical_batch_total_tokens = match args.max_physical_batch_total_tokens {
        None => max_batch_total_tokens,
        Some(size) => size,
    };
    let max_concurrent_requests = match args.max_concurrent_requests {
        None => max_batch_size * 2,
        Some(size) => size,
    };
    if args.max_input_tokens >= args.max_total_tokens {
        return Err(RouterError::ArgumentValidation(
            "`max_input_tokens` must be < `max_total_tokens`".to_string(),
        ));
    }
    if args.max_total_tokens > max_batch_total_tokens {
        return Err(RouterError::ArgumentValidation(
            "`max_total_tokens` must be <= `max_batch_total_tokens`".to_string(),
        ));
    }
    if max_batch_size * args.max_total_tokens > max_batch_total_tokens {
        return Err(RouterError::ArgumentValidation(
            "`max_batch_size` * `max_total_tokens` must be <= `max_batch_total_tokens`".to_string(),
        ));
    }

    // TODO: check if we use the same cache of Server
    // check if llamacpp is faster
    let tokenizer = {
        let token = std::env::var("HF_TOKEN")
            .or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN"))
            .ok();
        let params = FromPretrainedParameters {
            revision: args.revision.clone(),
            token: token,
            ..Default::default()
        };
        Tokenizer::from_pretrained(
            args.model_id.clone(),
            Some(params)
        )?
    };

    let (backend, ok, shutdown) = LlamacppBackend::new(
        LlamacppConfig {
            model_gguf:                      args.model_gguf,
            n_threads:                       n_threads,
            n_threads_batch:                 n_threads_batch,
            n_gpu_layers:                    args.n_gpu_layers,
            split_mode:                      args.split_mode,
            defrag_threshold:                args.defrag_threshold,
            numa:                            args.numa,
            use_mmap:                        args.use_mmap,
            use_mlock:                       args.use_mlock,
            flash_attention:                 args.flash_attention,
            type_k:                          args.type_k,
            type_v:                          args.type_v,
            offload_kqv:                     args.offload_kqv,
            max_batch_total_tokens:          max_batch_total_tokens,
            max_physical_batch_total_tokens: max_physical_batch_total_tokens,
            max_batch_size:                  max_batch_size,
            batch_timeout:                   tokio::time::Duration::from_millis(5),
        },
        tokenizer,
    );
    ok.await??;

    if cfg!(debug_assertions) {
        warn!("Graceful shutdown disabled!");
        let _ = tokio::task::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown.send(true);
        });
    }

    server::run(
        backend,
        max_concurrent_requests,
        0, // max_best_of
        0, // max_stop_sequences
        0, // max_top_n_tokens
        args.max_input_tokens,
        args.max_total_tokens,
        args.validation_workers,
        None, // api_key
        args.model_id, // tokenizer_name
        args.tokenizer_config_path,
        Some(args.revision),
        false, // trust_remote_code
        args.hostname,
        args.port,
        args.cors_allow_origin,
        args.ngrok,
        args.ngrok_authtoken,
        args.ngrok_edge,
        args.disable_grammar_support,
        args.max_client_batch_size,
        args.usage_stats,
        args.payload_limit,
    )
    .await?;
    Ok(())
}

#[derive(Debug, Error)]
enum RouterError {
    #[error("Argument validation error: {0}")]
    ArgumentValidation(String),
    #[error("Tokenizer error: {0}")]
    Tokenizer(#[from] tokenizers::Error),
    #[error("Backend error: {0}")]
    Backend(#[from] BackendError),
    #[error("WebServer error: {0}")]
    WebServer(#[from] server::WebServerError),
    #[error("Recv error: {0}")]
    RecvError(#[from] RecvError),
}
